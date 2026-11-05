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
use std::sync::{RwLock, RwLockWriteGuard};
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

/// Open a pool on `path` and release expired UTXO reservations in it.
fn open_pool(path: &Path) -> Result<Pool<SqliteConnectionManager>> {
    let pool = init_db(path.to_path_buf()).context("Failed to create database pool")?;
    match pool.get() {
        Ok(conn) => release_expired_reservations(&conn),
        Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
    }
    Ok(pool)
}

/// Close the shared pool and drop every key session.
fn close_pool(state: &mut DbState) {
    state.open = None;
    session::lock_all();
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
/// passphrase. When the wallet is already unlocked (for example by the other engine)
/// no key derivation runs and the passphrase is not checked again. Errors:
/// `Invalid Passphrase`, `No accounts found for this wallet`. Synchronous.
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

    // Argon2 runs here, outside the state lock.
    let unlocked = pool
        .get()
        .context("Failed to retrieve connection from pool")
        .and_then(|conn| {
            session::unlock(&conn, &wallet_name, &passphrase).map_err(|e| match e {
                WalletDbError::InvalidPassphrase => WalletError::InvalidPassphrase.into(),
                WalletDbError::InvalidInput(_) => WalletError::NoAccounts.into(),
                other => WalletError::database(other.to_string()).into(),
            })
        });
    if let Err(e) = unlocked {
        release_failed_unlock(&wallet_name, generation)?;
        return Err(e);
    }
    Ok(())
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
fn release_failed_unlock(wallet_name: &str, generation: u64) -> Result<()> {
    let mut state = write_state()?;
    if state.sessions.get(wallet_name).map(|s| s.generation) == Some(generation) {
        release_one(&mut state, wallet_name);
    }
    Ok(())
}

/// Release one [`unlock_wallet`] of `wallet_name`.
///
/// When its count reaches zero the key session is locked and the wallet's scans are
/// stopped (and waited for); when no wallet is open any more the pool is closed. A
/// wallet that is not unlocked is a no-op.
#[frb]
pub async fn lock_wallet(wallet_name: String) -> Result<()> {
    if release_session(&wallet_name)? {
        crate::api::scanner::stop_wallet_scans(&wallet_name).await?;
    }
    Ok(())
}

/// Take one off `wallet_name`'s open count. Returns whether it reached zero, in which
/// case its key session is locked (and the pool closed if it was the last wallet).
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
    session::lock(wallet_name);
    if state.sessions.is_empty() {
        close_pool(state);
    }
    true
}

/// Lock `wallet_name` for every holder at once (both engines) and stop its scans, as
/// delete and rename need.
pub(crate) async fn lock_wallet_everywhere(wallet_name: &str) -> Result<()> {
    {
        let mut state = write_state()?;
        if state.sessions.remove(wallet_name).is_some() && state.sessions.is_empty() {
            close_pool(&mut state);
        }
        session::lock(wallet_name);
    }
    crate::api::scanner::stop_wallet_scans(wallet_name).await
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
    let mut state = write_state()?;
    state.sessions.clear();
    close_pool(&mut state);
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
    let unlocked = session::is_unlocked(conn, wallet_name)
        .map_err(|e| WalletError::database(e.to_string()))?;
    if !unlocked {
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
        lock_wallet("db-twice".into()).await.unwrap();
        assert!(pool_open(), "one holder is left, so the pool stays open");
        assert!(unlocked("db-twice"), "and so does the session");

        lock_wallet("db-twice".into()).await.unwrap();
        assert!(!pool_open(), "the last lock closes the pool");
        assert!(!unlocked("db-twice"), "and the session");
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
