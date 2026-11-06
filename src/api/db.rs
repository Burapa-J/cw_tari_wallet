//! Database and wallet-session state.
//!
//! The process holds at most one SQLite pool, shared by every wallet in one database
//! file. Cake's Android background sync runs a second Flutter engine in this same
//! process, so every `static` here is shared by both engines while their Dart state is
//! not. The bridge therefore owns the pool's lifetime instead of either engine:
//!
//! - `set_database_path` records the database file without opening it. Functions that
//!   need the database while no wallet is unlocked open a temporary pool on that path.
//! - `unlock_wallet` opens the shared pool (if it is not open yet), unlocks the wallet's
//!   key session and adds one to the wallet's open count. Both engines can hold the
//!   same wallet open: each `unlock_wallet` needs its own `lock_wallet`.
//! - `lock_wallet` takes one off the count. At zero it locks the key session and stops
//!   that wallet's scans; when no wallet is open any more it closes the pool.
//!   `force_lock_wallet` does that for every holder at once.
//!
//! Lock order: a wallet's unlock gate ([`UNLOCK_GATES`]) before `DB_STATE`, never the
//! reverse. minotari's `session::lock` / `unlock` are only called with `DB_STATE`'s guard
//! released (they can wait for a passphrase change's Argon2) and with the wallet's gate
//! held, so a lock never interrupts an unlock of the same wallet and an unlock always
//! decides on the session state it sees.
//!
//! The lock is a synchronous [`RwLock`]; **no guard is ever held across an `.await`**.
//! The `r2d2` pool is `Send + Sync`, so cloning it out from under a short lock is safe.

use crate::api::error::WalletError;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::{
    expire_and_unlock_pending_transaction, find_expired_pending_transactions, get_account_by_name,
    AccountRow, WalletDbError,
};
use minotari_wallet::init_db;
use minotari_wallet::session;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::rusqlite::Connection;
use r2d2_sqlite::SqliteConnectionManager;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use zeroize::Zeroizing;

/// The open connection pool plus the on-disk path it was opened from.
struct Database {
    pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

struct DbState {
    /// The last path from `set_database_path` / `unlock_wallet`; kept after the pool closes.
    path: Option<PathBuf>,
    /// The shared pool, open while any wallet is unlocked.
    open: Option<Database>,
    /// Each unlocked wallet's open count.
    sessions: BTreeMap<String, Session>,
    /// The generation the next session entry gets.
    next_generation: u64,
}

/// A wallet's `unlock_wallet` calls not yet matched with `lock_wallet`.
struct Session {
    count: u32,
    /// Fresh each time the count rises from zero, so an unlock that fails later gives
    /// back only a count from its own generation, never a newer holder's.
    generation: u64,
}

static DB_STATE: RwLock<DbState> = RwLock::new(DbState {
    path: None,
    open: None,
    sessions: BTreeMap::new(),
    next_generation: 0,
});

const DIFFERENT_DATABASE: &str = "Another database is open; lock its wallets first";

fn write_state() -> Result<RwLockWriteGuard<'static, DbState>> {
    DB_STATE
        .write()
        .map_err(|_| WalletError::internal("Failed to lock DB_STATE for writing").into())
}

fn read_state() -> Result<RwLockReadGuard<'static, DbState>> {
    DB_STATE
        .read()
        .map_err(|_| WalletError::internal("Failed to lock DB_STATE for reading").into())
}

/// One gate per wallet, held by `unlock_wallet` across its passphrase check and by every
/// lock of that wallet's key session (see the module docs for the lock order).
static UNLOCK_GATES: Mutex<BTreeMap<String, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());

fn unlock_gate(wallet_name: &str) -> Arc<Mutex<()>> {
    UNLOCK_GATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(wallet_name.to_string())
        .or_default()
        .clone()
}

fn hold(gate: &Mutex<()>) -> MutexGuard<'_, ()> {
    gate.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Open a pool on `path` and release expired UTXO reservations in it.
fn open_pool(path: &Path) -> Result<Pool<SqliteConnectionManager>> {
    let pool = init_db(path.to_path_buf()).context("Failed to create database pool")?;
    match pool.get() {
        Ok(conn) => release_expired_reservations(&conn),
        Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
    }
    Ok(pool)
}

/// Close the shared pool. Every key session was locked by name when its count reached
/// zero, so there is none left to drop (and `lock_all` here could wipe a session an
/// unlock is storing right now).
fn close_pool(state: &mut DbState) {
    state.open = None;
}

/// Record `path` as the wallet database file, without opening it.
///
/// Call it before creating, restoring or importing a wallet when no wallet is unlocked
/// yet. [`unlock_wallet`] records its path too. Errors if a pool is open on a different
/// path. Synchronous.
#[frb]
pub fn set_database_path(path: String) -> Result<()> {
    let path = PathBuf::from(path);
    let mut state = write_state()?;
    if state.open.as_ref().is_some_and(|db| db.path != path) {
        return Err(WalletError::database(DIFFERENT_DATABASE).into());
    }
    state.path = Some(path);
    Ok(())
}

/// Unlock wallet `wallet_name` in the database at `db_path` for this process.
///
/// Opens the shared pool if none is open (a different path while wallets are unlocked
/// is an error), unlocks the wallet's key session with `passphrase` (**secret**) and
/// adds one to its open count. Address, signing, sending and scanning then need no
/// passphrase. If the wallet is force-locked, deleted or renamed while this call
/// runs, the lock wins: it errors with `Wallet is locked` and holds nothing. The
/// passphrase is always checked: when the wallet is already unlocked
/// (for example by the other engine) it is verified against the stored wallet (one
/// Argon2 run) and the existing session is shared. A failed unlock adds nothing.
/// Errors: `Invalid Passphrase`, `No accounts found for this wallet`,
/// `Wallet is locked`. Synchronous.
#[frb]
pub fn unlock_wallet(db_path: String, wallet_name: String, passphrase: String) -> Result<()> {
    let passphrase = Zeroizing::new(passphrase);
    let path = PathBuf::from(db_path);
    let (pool, generation) = {
        let mut state = write_state()?;
        let same_path = state.open.as_ref().map(|db| db.path == path);
        if same_path == Some(false) {
            if !state.sessions.is_empty() {
                return Err(WalletError::database(DIFFERENT_DATABASE).into());
            }
            close_pool(&mut state);
        }
        let pool = match &state.open {
            Some(db) => db.pool.clone(),
            None => {
                let pool = open_pool(&path)?;
                state.open = Some(Database {
                    pool: pool.clone(),
                    path: path.clone(),
                });
                pool
            }
        };
        state.path = Some(path);
        let generation = acquire_session(&mut state, &wallet_name);
        (pool, generation)
    };

    // Argon2 runs here, outside the state lock but inside the wallet's gate.
    let gate = unlock_gate(&wallet_name);
    let _held = hold(&gate);
    let unlocked = pool
        .get()
        .context("Failed to retrieve connection from pool")
        .and_then(|conn| check_and_unlock(&conn, &wallet_name, &passphrase));
    if let Err(e) = unlocked {
        release_failed_unlock(&wallet_name, generation)?;
        return Err(e);
    }
    confirm_still_counted(&wallet_name, generation)
}

/// After a successful passphrase check, with the wallet's gate held: if a force lock,
/// delete or rename removed this unlock's count meanwhile (and may have closed the
/// pool), the force wins and this unlock errors `Wallet is locked`, holding nothing.
/// The session it made or shared is locked (and the wallet's scans cancelled) only if
/// no newer holder is counted: the gate is not FIFO, so a holder that counted after the force may already have taken
/// the gate and unlocked, and its live session must survive.
fn confirm_still_counted(wallet_name: &str, generation: u64) -> Result<()> {
    if still_counted(wallet_name, generation)? {
        return Ok(());
    }
    lock_session_gate_held(wallet_name)?;
    Err(WalletError::Locked.into())
}

/// Whether `wallet_name`'s count still includes the unlock of `generation`.
fn still_counted(wallet_name: &str, generation: u64) -> Result<bool> {
    Ok(read_state()?
        .sessions
        .get(wallet_name)
        .map(|s| s.generation)
        == Some(generation))
}

/// Run blocking database or Argon2 work off the async runtime.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| WalletError::internal(format!("Blocking task failed: {e}")))?
}

/// The Dart-visible error for a failed unlock. `NotUnlocked` is minotari's answer to an
/// unlock interrupted by a concurrent lock, delete, rename or passphrase change.
fn map_unlock_error(e: WalletDbError) -> anyhow::Error {
    match e {
        WalletDbError::InvalidPassphrase | WalletDbError::DecryptionFailed(_) => {
            WalletError::InvalidPassphrase.into()
        }
        WalletDbError::InvalidInput(_) => WalletError::NoAccounts.into(),
        WalletDbError::NotUnlocked(_) => WalletError::Locked.into(),
        other => WalletError::database(other.to_string()).into(),
    }
}

/// Check `passphrase` and make sure `wallet_name` has a session. minotari's `unlock`
/// returns `Ok` without checking the passphrase when a session exists, so an existing
/// session is verified separately. Called with the wallet's gate held, so no lock of
/// this wallet runs in between.
fn check_and_unlock(conn: &Connection, wallet_name: &str, passphrase: &str) -> Result<()> {
    let map = map_unlock_error;
    if session::is_unlocked(conn, wallet_name).map_err(map)? {
        get_account_by_name(conn, wallet_name)
            .map_err(map)?
            .ok_or(WalletError::NoAccounts)?
            .verify_passphrase(passphrase)
            .map_err(map)?;
        // A passphrase change, delete or rename can still drop the session meanwhile.
        if session::is_unlocked(conn, wallet_name).map_err(map)? {
            return Ok(());
        }
    }
    session::unlock(conn, wallet_name, passphrase).map_err(map)
}

/// Add one to `wallet_name`'s open count, returning the session's generation.
fn acquire_session(state: &mut DbState, wallet_name: &str) -> u64 {
    let next = state.next_generation;
    let session = state
        .sessions
        .entry(wallet_name.to_string())
        .or_insert(Session {
            count: 0,
            generation: next,
        });
    if session.generation == next {
        state.next_generation = next.wrapping_add(1);
    }
    session.count = session.count.saturating_add(1);
    session.generation
}

/// Give back the count a failed [`unlock_wallet`] took, unless the wallet was locked
/// (and maybe unlocked again by another holder) meanwhile: that count is gone already.
/// Checked and released under one write lock, so no lock/unlock can fall in between.
/// If that was the last count, the session is locked and the wallet's scans cancelled,
/// as for any lock. Called with the wallet's gate held.
fn release_failed_unlock(wallet_name: &str, generation: u64) -> Result<()> {
    let reached_zero = {
        let mut state = write_state()?;
        state.sessions.get(wallet_name).map(|s| s.generation) == Some(generation)
            && release_one(&mut state, wallet_name)
    };
    if reached_zero {
        lock_session_gate_held(wallet_name)?;
    }
    Ok(())
}

/// Release one [`unlock_wallet`] of `wallet_name`.
///
/// Returns `true` when this call locked the wallet: its count reached zero, so its key
/// session is locked and its scans are stopped (and waited for), and when no wallet is
/// open any more the pool is closed. Returns `false` when other holders remain (for
/// example the other engine) or the wallet was not unlocked; nothing is locked then.
/// [`force_lock_wallet`] locks it for every holder.
#[frb]
pub async fn lock_wallet(wallet_name: String) -> Result<bool> {
    if !release_session(&wallet_name)? {
        return Ok(false);
    }
    let locked = lock_session_unless_held_async(&wallet_name).await?;
    if locked {
        crate::api::scanner::stop_wallet_scans(&wallet_name).await?;
    }
    Ok(locked)
}

/// Lock wallet `wallet_name` for every holder at once (both engines, and any count a
/// dead engine left behind) and stop (and wait for) its scans. Closes the pool when no
/// other wallet is open. A wallet that is not unlocked is a no-op. A later
/// `unlock_wallet` needs the passphrase again.
#[frb]
pub async fn force_lock_wallet(wallet_name: String) -> Result<()> {
    lock_wallet_everywhere(&wallet_name).await
}

/// Take one off `wallet_name`'s open count. Returns whether it reached zero (the pool
/// is closed if it was the last wallet); the caller then locks the session.
fn release_session(wallet_name: &str) -> Result<bool> {
    let mut state = write_state()?;
    Ok(release_one(&mut state, wallet_name))
}

/// [`release_session`] on an already-locked state.
fn release_one(state: &mut DbState, wallet_name: &str) -> bool {
    let Some(session) = state.sessions.get_mut(wallet_name) else {
        return false;
    };
    session.count = session.count.saturating_sub(1);
    if session.count > 0 {
        return false;
    }
    state.sessions.remove(wallet_name);
    if state.sessions.is_empty() {
        close_pool(state);
    }
    true
}

/// Lock `wallet_name`'s key session unless a new holder has counted it since its count
/// reached zero, taking the wallet's gate. Returns whether it locked. An unlock that
/// counted meanwhile waits on the gate and then decides on the locked session.
fn lock_session_unless_held(wallet_name: &str) -> Result<bool> {
    let gate = unlock_gate(wallet_name);
    let _held = hold(&gate);
    lock_session_gate_held(wallet_name)
}

/// [`lock_session_unless_held`] off the async runtime: the gate can be held for an
/// unlock's Argon2 run.
async fn lock_session_unless_held_async(wallet_name: &str) -> Result<bool> {
    let wallet_name = wallet_name.to_string();
    blocking(move || lock_session_unless_held(&wallet_name)).await
}

/// [`lock_session_unless_held`] with the gate already held.
///
/// Every transition that locks a session goes through here, so it also cancels the
/// wallet's scans (without waiting; `SCAN_STATE` only, safe under the gate): a scan
/// that saw the session must not keep running with its keys. Callers that can wait
/// then use `stop_wallet_scans`.
fn lock_session_gate_held(wallet_name: &str) -> Result<bool> {
    // Fail closed: if the state cannot be read, treat the wallet as having no holder
    // and lock it.
    let held = read_state()
        .map(|state| state.sessions.contains_key(wallet_name))
        .unwrap_or(false);
    if held {
        return Ok(false);
    }
    // `DB_STATE`'s guard is released: minotari's lock can wait for a passphrase change.
    session::lock(wallet_name);
    crate::api::scanner::cancel_wallet_scans(wallet_name)?;
    Ok(true)
}

/// Remove `wallet_name`'s entry as a force lock's state step does, returning the
/// generation it had.
#[cfg(test)]
pub(crate) fn force_remove_entry(wallet_name: &str) -> Option<u64> {
    write_state()
        .ok()?
        .sessions
        .remove(wallet_name)
        .map(|s| s.generation)
}

/// [`confirm_still_counted`] with the wallet's gate held, as `unlock_wallet` calls it.
#[cfg(test)]
pub(crate) fn confirm_still_counted_for_test(wallet_name: &str, generation: u64) -> Result<()> {
    let gate = unlock_gate(wallet_name);
    let _held = hold(&gate);
    confirm_still_counted(wallet_name, generation)
}

/// Lock `wallet_name` for every holder at once (both engines) and stop its scans, as
/// [`force_lock_wallet`], delete and rename need.
pub(crate) async fn lock_wallet_everywhere(wallet_name: &str) -> Result<()> {
    {
        let mut state = write_state()?;
        if state.sessions.remove(wallet_name).is_some() && state.sessions.is_empty() {
            close_pool(&mut state);
        }
    }
    lock_session_unless_held_async(wallet_name).await?;
    crate::api::scanner::stop_wallet_scans(wallet_name).await
}

/// Run `read_keys`, which reads `wallet_name`'s keys through its session by passing an
/// empty passphrase, and report `Wallet is locked` if the session is gone afterwards. A
/// lock between the caller's [`unlocked_account`] check and `read_keys` makes minotari
/// fall back to decrypting with the empty passphrase, and that result (error or not)
/// must not be used.
pub(crate) fn with_session<T>(
    conn: &Connection,
    wallet_name: &str,
    read_keys: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let result = read_keys();
    if !is_unlocked(conn, wallet_name)? {
        return Err(WalletError::Locked.into());
    }
    result
}

/// Whether `wallet_name`'s key session is known to be gone (a database error reads as
/// "not gone", so it never masks the original error).
pub(crate) fn session_gone(wallet_name: &str) -> bool {
    matches!(
        get_db_connection().and_then(|conn| is_unlocked(&conn, wallet_name)),
        Ok(false)
    )
}

/// Whether `wallet_name` has a key session the key paths will use.
pub(crate) fn is_unlocked(conn: &Connection, wallet_name: &str) -> Result<bool> {
    session::is_unlocked(conn, wallet_name).map_err(|e| WalletError::database(e.to_string()).into())
}

/// Checkpoint the database's write-ahead log into the main file and truncate it, so a
/// copy of the database file is consistent.
///
/// Uses the open pool, or opens one temporarily on the path from
/// [`set_database_path`] / [`unlock_wallet`], so it works whether or not a wallet is
/// unlocked. Errors if another connection kept the checkpoint from finishing.
/// Synchronous.
#[frb]
pub fn checkpoint_database() -> Result<()> {
    let conn = get_db_connection()?;
    let busy: i64 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .map_err(|e| WalletError::database(e.to_string()))?;
    if busy != 0 {
        return Err(WalletError::database("The database is busy; try the checkpoint again").into());
    }
    Ok(())
}

/// Cancel every scan, drop every key session and close the pool. Idempotent.
#[cfg(test)]
pub(crate) fn disconnect_database() -> Result<()> {
    crate::api::scanner::cancel_all_scans()?;
    {
        let mut state = write_state()?;
        state.sessions.clear();
        close_pool(&mut state);
    }
    session::lock_all();
    Ok(())
}

/// Forget the recorded path too, so the accessors report "not initialized".
#[cfg(test)]
fn forget_database_path() {
    if let Ok(mut state) = DB_STATE.write() {
        state.path = None;
    }
}

/// Release UTXO reservations whose lock has expired, best-effort.
///
/// Heals inputs left `Locked` by a send that never got recorded (an app kill between
/// reserving and recording). Only `Pending` rows past `expires_at` are released, each
/// re-checked under its own write, so a recorded send is never touched. A failure is
/// logged rather than returned: the database open, send and scan that run it must not
/// start failing.
pub(crate) fn release_expired_reservations(conn: &Connection) {
    let expired = match find_expired_pending_transactions(conn) {
        Ok(expired) => expired,
        Err(e) => {
            log::warn!("Failed to release expired UTXO reservations: {e}");
            return;
        }
    };
    for tx in expired {
        if let Err(e) = expire_and_unlock_pending_transaction(conn, &tx.id) {
            log::warn!("Failed to release expired UTXO reservations: {e}");
        }
    }
}

/// The open pool, or a temporary one on the recorded path.
pub(crate) fn get_db_pool() -> Result<Pool<SqliteConnectionManager>> {
    let path = {
        let state = DB_STATE
            .read()
            .map_err(|_| WalletError::internal("Failed to lock DB_STATE for reading"))?;
        if let Some(db) = &state.open {
            return Ok(db.pool.clone());
        }
        state.path.clone().ok_or(WalletError::NotInitialized)?
    };
    init_db(path).context("Failed to create database pool")
}

pub(crate) fn get_db_connection() -> Result<PooledConnection<SqliteConnectionManager>> {
    get_db_pool()?
        .get()
        .context("Failed to retrieve connection from pool")
}

pub(crate) fn get_db_path() -> Result<PathBuf> {
    let state = DB_STATE
        .read()
        .map_err(|_| WalletError::internal("Failed to lock DB_STATE for reading"))?;
    if let Some(db) = &state.open {
        return Ok(db.path.clone());
    }
    Ok(state.path.clone().ok_or(WalletError::NotInitialized)?)
}

/// The account row of `wallet_name`, which must be unlocked (`Wallet is locked`
/// otherwise). Key paths then pass an empty passphrase so minotari uses the session.
pub(crate) fn unlocked_account(conn: &Connection, wallet_name: &str) -> Result<AccountRow> {
    let account = get_account_by_name(conn, wallet_name)
        .map_err(|e| WalletError::database(e.to_string()))?
        .ok_or(WalletError::NoAccounts)?;
    if !is_unlocked(conn, wallet_name)? {
        return Err(WalletError::Locked.into());
    }
    Ok(account)
}

/// Test-only serialization lock for the process-global slots (`db::DB_STATE`,
/// `scanner::SCAN_STATE` and the minotari key sessions). Every test that touches them
/// holds it. A `tokio::sync::Mutex` so its guard can be held across `.await`.
#[cfg(test)]
pub(crate) static GLOBAL_STATE_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    //! Lifecycle tests for the database / session state. They create throwaway
    //! databases in temp dirs and hold [`GLOBAL_STATE_TEST_LOCK`], since they mutate
    //! the process-global slots.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use minotari_wallet::db::create_account;
    use tari_common_types::seeds::cipher_seed::CipherSeed;
    use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};

    const PASSPHRASE: &str = "pass";

    /// A database at `dir/wallet.db` holding a seed wallet per name.
    fn new_database(dir: &tempfile::TempDir, wallets: &[&str]) -> String {
        let path = dir.path().join("wallet.db");
        let pool = init_db(path.clone()).unwrap();
        let conn = pool.get().unwrap();
        for name in wallets {
            let wallet = WalletType::SeedWords(
                SeedWordsWallet::construct_new(CipherSeed::random()).unwrap(),
            );
            create_account(&conn, name, &wallet, PASSPHRASE).unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    fn pool_open() -> bool {
        DB_STATE.read().unwrap().open.is_some()
    }

    fn unlocked(name: &str) -> bool {
        session::is_unlocked(&get_db_connection().unwrap(), name).unwrap()
    }

    #[tokio::test]
    async fn accessors_report_not_initialized_before_init() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        forget_database_path();

        let conn_err = get_db_connection().unwrap_err();
        assert_eq!(conn_err.to_string(), "Database is not initialized");
        let path_err = get_db_path().unwrap_err();
        assert_eq!(path_err.to_string(), "Database is not initialized");
        let pool_err = get_db_pool().unwrap_err();
        assert_eq!(pool_err.to_string(), "Database is not initialized");
        let checkpoint_err = checkpoint_database().unwrap_err();
        assert_eq!(checkpoint_err.to_string(), "Database is not initialized");
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_closes_the_pool() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        disconnect_database().unwrap();
        assert!(!pool_open());
    }

    #[tokio::test]
    async fn two_unlocks_need_two_locks() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-twice"]);

        unlock_wallet(path.clone(), "db-twice".into(), PASSPHRASE.into()).unwrap();
        unlock_wallet(path, "db-twice".into(), PASSPHRASE.into()).unwrap();
        assert!(
            !lock_wallet("db-twice".into()).await.unwrap(),
            "another holder remains, so this lock did not lock"
        );
        assert!(pool_open(), "one holder is left, so the pool stays open");
        assert!(unlocked("db-twice"), "and so does the session");

        assert!(
            lock_wallet("db-twice".into()).await.unwrap(),
            "the last lock locks"
        );
        assert!(!pool_open(), "the last lock closes the pool");
        assert!(!unlocked("db-twice"), "and the session");
        assert!(
            !lock_wallet("db-twice".into()).await.unwrap(),
            "a wallet that is not unlocked is not locked again"
        );
    }

    fn count(wallet: &str) -> Option<u32> {
        DB_STATE
            .read()
            .unwrap()
            .sessions
            .get(wallet)
            .map(|s| s.count)
    }

    /// minotari's unlock does not check the passphrase when a session exists; ours does,
    /// and a refused unlock adds no count.
    #[tokio::test]
    async fn a_wrong_passphrase_is_refused_while_unlocked() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-verify"]);

        unlock_wallet(path.clone(), "db-verify".into(), PASSPHRASE.into()).unwrap();
        let err = unlock_wallet(path.clone(), "db-verify".into(), "wrong".into()).unwrap_err();
        assert_eq!(err.to_string(), "Invalid Passphrase");
        assert_eq!(count("db-verify"), Some(1), "count unchanged");
        assert!(unlocked("db-verify"), "the existing session is untouched");

        unlock_wallet(path, "db-verify".into(), PASSPHRASE.into()).unwrap();
        assert_eq!(
            count("db-verify"),
            Some(2),
            "a right passphrase shares the session"
        );
        force_lock_wallet("db-verify".into()).await.unwrap();
    }

    /// Wait (briefly polling) until `done` holds.
    async fn wait_until(mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    /// A force lock that runs while an unlock has counted but not yet got the wallet's
    /// gate wins: the unlock errors `Wallet is locked`, and no session, count or open
    /// pool is left behind.
    // Holding the gate across the waits is the point: the unlock runs on its own
    // thread and the force lock's gate wait on the blocking pool.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_force_lock_during_an_unlock_wins() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-force-race"]);

        // Hold the gate so the unlock stops between counting and the gate.
        let gate = unlock_gate("db-force-race");
        let held = hold(&gate);
        let unlock = std::thread::spawn(move || {
            unlock_wallet(path, "db-force-race".into(), PASSPHRASE.into())
        });
        wait_until(|| count("db-force-race") == Some(1)).await;

        // The force lock removes the count (closing the pool), then waits on the gate.
        let force = tokio::spawn(force_lock_wallet("db-force-race".into()));
        wait_until(|| count("db-force-race").is_none()).await;
        assert!(!pool_open(), "the force lock closed the pool");
        drop(held);

        force.await.unwrap().unwrap();
        let err = unlock.join().unwrap().unwrap_err();
        assert_eq!(err.to_string(), "Wallet is locked");
        assert_eq!(count("db-force-race"), None, "no count left behind");
        assert!(!pool_open(), "the pool stays closed");
        let pool = init_db(dir.path().join("wallet.db")).unwrap();
        assert!(
            !session::is_unlocked(&pool.get().unwrap(), "db-force-race").unwrap(),
            "no session left behind"
        );
    }

    /// The gate is not FIFO: after a force lock removes unlock B's count, a newer
    /// unlock C can count, take the gate first and unlock. When B then gets the gate it
    /// must error `Wallet is locked` without wiping C's live session or count. Driven
    /// step by step, since the gate's order cannot be forced.
    #[tokio::test]
    async fn a_force_lock_loser_leaves_a_newer_holders_session() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-not-fifo"]);

        // B counts (as `unlock_wallet` does before waiting on the gate).
        let b = acquire_session(&mut write_state().unwrap(), "db-not-fifo");
        // The force lock removes the entry, B's count included.
        write_state().unwrap().sessions.remove("db-not-fifo");
        // C counts, takes the gate first and unlocks.
        unlock_wallet(path, "db-not-fifo".into(), PASSPHRASE.into()).unwrap();
        assert_ne!(generation("db-not-fifo"), Some(b));

        // B gets the gate after its passphrase check succeeded.
        let gate = unlock_gate("db-not-fifo");
        let held = hold(&gate);
        let err = confirm_still_counted("db-not-fifo", b).unwrap_err();
        drop(held);
        assert_eq!(err.to_string(), "Wallet is locked");
        assert_eq!(count("db-not-fifo"), Some(1), "C's count survives");
        assert!(unlocked("db-not-fifo"), "and so does C's session");
        assert!(pool_open());

        assert!(lock_wallet("db-not-fifo".into()).await.unwrap());
        assert!(!pool_open());
    }

    /// `force_lock_wallet` locks for every holder (including a count a dead engine left
    /// behind) and closes the pool when no other wallet is open.
    #[tokio::test]
    async fn force_lock_releases_every_holder() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-force", "db-force-other"]);

        unlock_wallet(path.clone(), "db-force".into(), PASSPHRASE.into()).unwrap();
        unlock_wallet(path.clone(), "db-force".into(), PASSPHRASE.into()).unwrap();
        unlock_wallet(path.clone(), "db-force-other".into(), PASSPHRASE.into()).unwrap();

        force_lock_wallet("db-force".into()).await.unwrap();
        assert_eq!(count("db-force"), None);
        assert!(!unlocked("db-force"));
        assert!(pool_open(), "another wallet still holds the pool");
        assert!(unlocked("db-force-other"), "and keeps its session");

        force_lock_wallet("db-force-other".into()).await.unwrap();
        assert!(!pool_open(), "no wallet is open any more");
        // A wallet that is not unlocked is a no-op.
        force_lock_wallet("db-force".into()).await.unwrap();
    }

    /// A key path that lost its session meanwhile reports `Wallet is locked`, even when
    /// minotari's empty-passphrase fallback would decrypt the wallet.
    #[tokio::test]
    async fn a_key_read_after_the_session_is_gone_is_locked() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.db");
        let pool = init_db(path.clone()).unwrap();
        let conn = pool.get().unwrap();
        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(CipherSeed::random()).unwrap());
        create_account(&conn, "db-empty", &wallet, "").unwrap();
        let account = get_account_by_name(&conn, "db-empty").unwrap().unwrap();

        // Locked: the empty passphrase would decrypt it, but the result is refused.
        assert!(account
            .get_address(tari_common::configuration::Network::LocalNet, "")
            .is_ok());
        let err = with_session(&conn, "db-empty", || {
            Ok(account.get_address(tari_common::configuration::Network::LocalNet, "")?)
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "Wallet is locked");

        unlock_wallet(
            path.to_string_lossy().into_owned(),
            "db-empty".into(),
            "".into(),
        )
        .unwrap();
        with_session(&conn, "db-empty", || {
            Ok(account.get_address(tari_common::configuration::Network::LocalNet, "")?)
        })
        .expect("unlocked");
        drop(conn);
        lock_wallet("db-empty".into()).await.unwrap();
    }

    #[tokio::test]
    async fn unlock_after_the_last_lock_reopens_the_pool() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-reopen-a", "db-reopen-b"]);

        unlock_wallet(path.clone(), "db-reopen-a".into(), PASSPHRASE.into()).unwrap();
        unlock_wallet(path.clone(), "db-reopen-b".into(), PASSPHRASE.into()).unwrap();
        lock_wallet("db-reopen-a".into()).await.unwrap();
        assert!(pool_open(), "wallet b still holds the pool");
        assert!(!unlocked("db-reopen-a"));
        assert!(unlocked("db-reopen-b"));
        lock_wallet("db-reopen-b".into()).await.unwrap();
        assert!(!pool_open());

        unlock_wallet(path, "db-reopen-a".into(), PASSPHRASE.into()).unwrap();
        assert!(pool_open(), "a later unlock reopens the pool");
        assert!(unlocked("db-reopen-a"));
        lock_wallet("db-reopen-a".into()).await.unwrap();
        // A lock with no unlock left is a no-op.
        lock_wallet("db-reopen-a".into()).await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_unlock_holds_nothing() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-wrong"]);

        let err = unlock_wallet(path.clone(), "db-wrong".into(), "nope".into()).unwrap_err();
        assert_eq!(err.to_string(), "Invalid Passphrase");
        let err = unlock_wallet(path, "db-missing".into(), PASSPHRASE.into()).unwrap_err();
        assert_eq!(err.to_string(), "No accounts found for this wallet");
        assert!(!pool_open());
        assert!(DB_STATE.read().unwrap().sessions.is_empty());
    }

    #[test]
    fn interrupted_unlock_is_locked() {
        assert_eq!(
            map_unlock_error(WalletDbError::NotUnlocked("W".to_string())).to_string(),
            "Wallet is locked"
        );
        assert_eq!(
            map_unlock_error(WalletDbError::InvalidPassphrase).to_string(),
            "Invalid Passphrase"
        );
        assert_eq!(
            map_unlock_error(WalletDbError::InvalidInput("x".to_string())).to_string(),
            "No accounts found for this wallet"
        );
    }

    fn generation(wallet: &str) -> Option<u64> {
        DB_STATE
            .read()
            .unwrap()
            .sessions
            .get(wallet)
            .map(|s| s.generation)
    }

    /// Engine 1's unlock takes a count and runs Argon2; meanwhile the wallet is locked
    /// (pool closed) and engine 2 unlocks it again. Engine 1's unlock then fails: it
    /// must not take engine 2's count, lock the wallet or close the pool.
    #[tokio::test]
    async fn a_failed_unlock_does_not_release_a_newer_holder() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-race"]);

        // Engine 1 takes its count (as `unlock_wallet` does before Argon2).
        unlock_wallet(path.clone(), "db-race".into(), PASSPHRASE.into()).unwrap();
        let engine1 = generation("db-race").unwrap();
        // The wallet is locked meanwhile, then engine 2 unlocks it.
        lock_wallet("db-race".into()).await.unwrap();
        assert!(!pool_open());
        unlock_wallet(path.clone(), "db-race".into(), PASSPHRASE.into()).unwrap();
        let engine2 = generation("db-race").unwrap();
        assert_ne!(engine1, engine2, "a new session gets a new generation");

        // Engine 1's unlock fails now.
        release_failed_unlock("db-race", engine1).unwrap();
        assert!(pool_open(), "engine 2's pool stays open");
        assert!(unlocked("db-race"), "and its session");
        assert_eq!(DB_STATE.read().unwrap().sessions["db-race"].count, 1);

        // A second holder in the same generation shares it; a failure there gives back
        // only its own count.
        unlock_wallet(path, "db-race".into(), PASSPHRASE.into()).unwrap();
        assert_eq!(generation("db-race"), Some(engine2));
        release_failed_unlock("db-race", engine2).unwrap();
        assert_eq!(DB_STATE.read().unwrap().sessions["db-race"].count, 1);
        assert!(unlocked("db-race"));

        lock_wallet("db-race".into()).await.unwrap();
        assert!(!pool_open());
    }

    #[tokio::test]
    async fn a_different_path_while_unlocked_is_refused() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-path"]);
        let other = new_database(&other_dir, &["db-path-other"]);

        unlock_wallet(path.clone(), "db-path".into(), PASSPHRASE.into()).unwrap();
        let err =
            unlock_wallet(other.clone(), "db-path-other".into(), PASSPHRASE.into()).unwrap_err();
        assert!(
            err.to_string().contains("Another database is open"),
            "{err}"
        );
        assert!(set_database_path(other).is_err());
        set_database_path(path).unwrap();
        lock_wallet("db-path".into()).await.unwrap();
    }

    fn wal_len(db_path: &str) -> u64 {
        std::fs::metadata(format!("{db_path}-wal")).map_or(0, |m| m.len())
    }

    #[tokio::test]
    async fn checkpoint_truncates_the_wal_with_or_without_a_session() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["db-checkpoint"]);

        // With no session: a temporary pool on the recorded path.
        set_database_path(path.clone()).unwrap();
        let conn = get_db_connection().unwrap();
        conn.execute_batch("CREATE TABLE scratch (v); INSERT INTO scratch VALUES (1);")
            .unwrap();
        assert!(wal_len(&path) > 0, "the write went to the wal");
        checkpoint_database().unwrap();
        assert_eq!(wal_len(&path), 0, "the checkpoint truncated the wal");

        // With a session: the open pool.
        unlock_wallet(path.clone(), "db-checkpoint".into(), PASSPHRASE.into()).unwrap();
        conn.execute("INSERT INTO scratch VALUES (2)", []).unwrap();
        assert!(wal_len(&path) > 0);
        checkpoint_database().unwrap();
        assert_eq!(wal_len(&path), 0);
        drop(conn);
        lock_wallet("db-checkpoint".into()).await.unwrap();
    }
}
