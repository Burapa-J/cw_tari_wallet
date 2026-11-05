//! Wallet scans, one slot per wallet.
//!
//! Each scan belongs to the [`ScanHandle`] it was started with. Different wallets scan
//! at the same time with separate `Scanner`s. A second start for the same wallet
//! replaces the first (which ends with `Superseded`), except that a Background start
//! never replaces a running Foreground scan. Nothing stops a scan except its own
//! handle, a closed stream, a Foreground start over a Background scan of the same
//! wallet, and the wallet's own lock / maintenance (`lock_wallet`, `delete_wallet`,
//! `rename_wallet`, `change_passphrase`, `abandon_send`).
//!
//! The `std::sync::RwLock` guard is **never held across an `.await`**.

use crate::api::db::{
    get_db_connection, get_db_path, is_unlocked, release_expired_reservations, unlocked_account,
};
use crate::api::error::WalletError;
use crate::api::transactions::DisplayedTransactionDto;
use crate::domain::text::strip_format_chars;
use crate::frb_generated::StreamSink;
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::get_account_by_name;
use minotari_wallet::scan::reorg::rollback_from_height;
use minotari_wallet::scan::{
    DisplayedTransactionsEvent, ReorgDetectedEvent, TransactionsUpdatedEvent,
};
use minotari_wallet::{EventReceiver, ProcessingEvent, ScanMode, ScanStatusEvent, Scanner};
use r2d2_sqlite::rusqlite::TransactionBehavior;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Names one scan. Get it from [`create_scan_handle`], start the scan with it
/// ([`start_scan`] or [`rescan`]) and stop that scan with [`stop_scan`]. A handle
/// starts one scan only.
#[frb]
#[derive(Debug, Clone)]
pub struct ScanHandle {
    pub wallet_name: String,
    pub id: u64,
}

/// Who a scan is for. A Foreground start replaces a Background scan of the same
/// wallet; a Background start during a Foreground scan is refused.
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPriority {
    Foreground,
    Background,
}

/// A running scan, in its wallet's slot.
struct ScanController {
    id: u64,
    priority: ScanPriority,
    token: CancellationToken,
    /// Set when a newer scan of the same wallet replaced this one.
    superseded: Arc<AtomicBool>,
    /// Cancelled once the scan has sent its terminal event and returned.
    stopped: CancellationToken,
}

struct ScanState {
    /// Handles from `create_scan_handle` that have not started a scan.
    issued: BTreeSet<u64>,
    /// Handles stopped before their scan started.
    stopped_before_start: BTreeSet<u64>,
    /// Scans started but not in their slot yet (waiting on [`MAINTENANCE`]), by id.
    starting: BTreeMap<u64, String>,
    /// The running scan of each wallet.
    running: BTreeMap<String, ScanController>,
}

static SCAN_STATE: RwLock<ScanState> = RwLock::new(ScanState {
    issued: BTreeSet::new(),
    stopped_before_start: BTreeSet::new(),
    starting: BTreeMap::new(),
    running: BTreeMap::new(),
});

/// One lock per wallet, held by a maintenance operation on it (`abandon_send`,
/// `change_passphrase`) from stopping the wallet's scan until its DB write is done, and
/// taken briefly by a starting scan of that wallet before it takes its slot. A stop of
/// that wallet meanwhile removes the starting scan from [`ScanState::starting`], so it
/// never runs. Other wallets' scans are not delayed. The map's `std` guard is only held
/// to look up the lock, never across an `.await`.
static MAINTENANCE: std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>> =
    std::sync::Mutex::new(BTreeMap::new());

/// `wallet_name`'s maintenance lock.
fn maintenance_lock(wallet_name: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
    let mut locks = MAINTENANCE
        .lock()
        .map_err(|_| WalletError::internal("Failed to lock MAINTENANCE"))?;
    Ok(locks.entry(wallet_name.to_string()).or_default().clone())
}

static SCAN_ID: AtomicU64 = AtomicU64::new(0);

fn write_state() -> Result<RwLockWriteGuard<'static, ScanState>> {
    SCAN_STATE
        .write()
        .map_err(|_| WalletError::internal("Failed to acquire lock").into())
}

/// Start a maintenance operation on `wallet_name`: no scan of it can take its slot
/// until the guard is dropped. Stop the wallet's scan ([`stop_wallet_scans`]) after
/// taking it.
pub(crate) async fn begin_maintenance(
    wallet_name: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    Ok(maintenance_lock(wallet_name)?.lock_owned().await)
}

/// A new handle for one scan of `wallet_name`. Synchronous.
#[frb(sync)]
pub fn create_scan_handle(wallet_name: String) -> Result<ScanHandle> {
    let id = SCAN_ID.fetch_add(1, Ordering::SeqCst);
    write_state()?.issued.insert(id);
    Ok(ScanHandle { wallet_name, id })
}

/// Stop the scan started with `handle` and wait until it has stopped.
///
/// Only that scan: a handle whose scan already finished or was superseded does
/// nothing. A handle whose scan has not started yet stops it before it runs.
#[frb]
pub async fn stop_scan(handle: ScanHandle) -> Result<()> {
    let stopped = {
        let mut state = write_state()?;
        if state.issued.remove(&handle.id) {
            state.stopped_before_start.insert(handle.id);
            return Ok(());
        }
        if state.starting.remove(&handle.id).is_some() {
            return Ok(());
        }
        match state.running.get(&handle.wallet_name) {
            Some(controller) if controller.id == handle.id => {
                controller.token.cancel();
                controller.stopped.clone()
            }
            _ => return Ok(()),
        }
    };
    stopped.cancelled().await;
    Ok(())
}

/// Stop every scan of `wallet_name`, running or about to start, and wait for the
/// running one to stop. Other wallets' scans are untouched.
pub(crate) async fn stop_wallet_scans(wallet_name: &str) -> Result<()> {
    if let Some(stopped) = cancel_wallet_scans(wallet_name)? {
        stopped.cancelled().await;
    }
    Ok(())
}

/// Cancel `wallet_name`'s running scan and any of its scans not started yet, without
/// waiting. Returns the running scan's `stopped` token, if there is one.
pub(crate) fn cancel_wallet_scans(wallet_name: &str) -> Result<Option<CancellationToken>> {
    let mut state = write_state()?;
    state.starting.retain(|_, wallet| wallet != wallet_name);
    Ok(state.running.get(wallet_name).map(|controller| {
        controller.token.cancel();
        controller.stopped.clone()
    }))
}

/// Cancel every scan without waiting.
#[cfg(test)]
pub(crate) fn cancel_all_scans() -> Result<()> {
    let mut state = write_state()?;
    state.starting.clear();
    for controller in state.running.values() {
        controller.token.cancel();
    }
    Ok(())
}

/// Clear `wallet_name`'s slot **only if** it still holds scan `id`.
fn clear_scan_if_current(wallet_name: &str, id: u64) {
    let Ok(mut state) = SCAN_STATE.write() else {
        return;
    };
    if state.running.get(wallet_name).is_some_and(|c| c.id == id) {
        state.running.remove(wallet_name);
    }
}

/// An event streamed by [`start_scan`] and [`rescan`].
///
/// `Progress.tip_height` is the height the current pass scans towards. `Waiting` means
/// the scan has caught up with the tip and is polling (continuous mode). Every stream
/// ends with exactly one terminal event: `Completed` (a one-shot scan reached the tip),
/// `Cancelled` (stopped with `stop_scan`, by the wallet's lock or maintenance, or its
/// stream was closed), `Superseded` (a newer scan of the same wallet replaced it),
/// `BudgetExhausted` (`deadline_seconds` passed or `max_blocks` were scanned before the
/// tip) or `Error` (the scan failed; it also resolves to `Err`).
#[frb]
#[derive(Debug, Clone)]
pub enum ScanEventDto {
    Started {
        account_id: i64,
        from_height: u64,
    },
    Progress {
        account_id: i64,
        current_height: u64,
        tip_height: u64,
        blocks_scanned: u64,
    },
    Completed {
        account_id: i64,
        final_height: u64,
        total_blocks_scanned: u64,
    },
    Waiting {
        account_id: i64,
        resume_in_seconds: u64,
    },
    MoreBlocksAvailable {
        account_id: i64,
        last_scanned_height: u64,
    },
    TransactionsReady(TransactionsReadyDto),
    TransactionsUpdated(TransactionsUpdatedDto),
    ReorgDetected(ReorgDetectedDto),
    Cancelled,
    Superseded,
    BudgetExhausted {
        account_id: i64,
        last_scanned_height: u64,
        tip_height: u64,
    },
    Error(String),
}

/// A blockchain reorganization the scan detected and rolled back, carried inside
/// [`ScanEventDto::ReorgDetected`].
///
/// Blocks from `reorg_from_height` up were rolled back and the chain is now at
/// `new_height`. `cancelled_transaction_ids` are the wallet transactions the reorg
/// removed; `reorganized_transactions` are those whose state changed (show them as
/// pending again, or drop them).
#[frb]
#[derive(Debug, Clone)]
pub struct ReorgDetectedDto {
    pub account_id: i64,
    pub reorg_from_height: u64,
    pub new_height: u64,
    pub blocks_rolled_back: u64,
    pub cancelled_transaction_ids: Vec<String>,
    pub reorganized_transactions: Vec<DisplayedTransactionDto>,
}

impl From<ReorgDetectedEvent> for ReorgDetectedDto {
    fn from(e: ReorgDetectedEvent) -> Self {
        Self {
            account_id: e.account_id,
            reorg_from_height: e.reorg_from_height,
            new_height: e.new_height,
            blocks_rolled_back: e.blocks_rolled_back,
            cancelled_transaction_ids: e.cancelled_transaction_ids,
            reorganized_transactions: e
                .reorganized_displayed_transactions
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

/// Map a raw scanner [`ProcessingEvent`] to a [`ScanEventDto`].
///
/// Returns `None` for events with no Dto. `BlockProcessed` is per-block bookkeeping
/// Dart has never been shown. `FastSyncPhaseStarted` / `FastSyncPhaseCompleted` are
/// emitted **only** under `ScanMode::FastSync` and by the separate backfill pass,
/// neither of which this bridge selects, so dropping them is observationally a no-op.
///
/// Both matches are deliberately **exhaustive** (no `_` arm): a new upstream variant
/// may well be balance- or security-relevant, and must fail the build so the
/// drop-or-surface decision is made deliberately.
fn map_processing_event(event: ProcessingEvent) -> Option<ScanEventDto> {
    let status = match event {
        ProcessingEvent::ScanStatus(status) => status,
        ProcessingEvent::TransactionsReady(e) => {
            return Some(ScanEventDto::TransactionsReady(e.into()))
        }
        ProcessingEvent::TransactionsUpdated(e) => {
            return Some(ScanEventDto::TransactionsUpdated(e.into()))
        }
        ProcessingEvent::ReorgDetected(e) => return Some(ScanEventDto::ReorgDetected(e.into())),
        ProcessingEvent::BlockProcessed(_) => return None,
    };
    let dto = match status {
        ScanStatusEvent::FastSyncPhaseStarted { .. }
        | ScanStatusEvent::FastSyncPhaseCompleted { .. } => {
            log::warn!(
                "Dropping a fast-sync ScanStatusEvent with no Dto representation; this bridge \
                 never selects ScanMode::FastSync, so receiving one means upstream changed \
                 which scan path the other modes take"
            );
            return None;
        }
        ScanStatusEvent::Started {
            account_id,
            from_height,
        } => ScanEventDto::Started {
            account_id,
            from_height,
        },
        ScanStatusEvent::Progress {
            account_id,
            current_height,
            tip_height,
            blocks_scanned,
        } => ScanEventDto::Progress {
            account_id,
            current_height,
            tip_height,
            blocks_scanned,
        },
        ScanStatusEvent::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        } => ScanEventDto::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        },
        ScanStatusEvent::Waiting {
            account_id,
            resume_in,
        } => ScanEventDto::Waiting {
            account_id,
            resume_in_seconds: resume_in.as_secs(),
        },
        ScanStatusEvent::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        } => ScanEventDto::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        },
        ScanStatusEvent::Cancelled { .. } => ScanEventDto::Cancelled,
        ScanStatusEvent::BudgetExhausted {
            account_id,
            last_scanned_height,
            tip_height,
        } => ScanEventDto::BudgetExhausted {
            account_id,
            last_scanned_height,
            tip_height,
        },
    };
    Some(dto)
}

/// Whether `event` ends a scan stream.
fn is_terminal(event: &ScanEventDto) -> bool {
    matches!(
        event,
        ScanEventDto::Completed { .. }
            | ScanEventDto::Cancelled
            | ScanEventDto::Superseded
            | ScanEventDto::BudgetExhausted { .. }
            | ScanEventDto::Error(_)
    )
}

/// Newly discovered transactions, carried inside
/// [`ScanEventDto::TransactionsReady`].
///
/// `block_height` is the block they were found at (when known); `is_initial_sync`
/// is `true` while catching up history versus live tip-following.
#[frb]
#[derive(Debug, Clone)]
pub struct TransactionsReadyDto {
    pub account_id: i64,
    pub transactions: Vec<DisplayedTransactionDto>,
    pub block_height: Option<u64>,
    pub is_initial_sync: bool,
}

impl From<DisplayedTransactionsEvent> for TransactionsReadyDto {
    fn from(e: DisplayedTransactionsEvent) -> Self {
        Self {
            account_id: e.account_id,
            transactions: e.transactions.into_iter().map(Into::into).collect(),
            block_height: e.block_height,
            is_initial_sync: e.is_initial_sync,
        }
    }
}

/// Status updates to already-known transactions (e.g. newly confirmed), carried
/// inside [`ScanEventDto::TransactionsUpdated`]. This is how a send's confirmation is
/// reported.
#[frb]
#[derive(Debug, Clone)]
pub struct TransactionsUpdatedDto {
    pub account_id: i64,
    pub updated_transactions: Vec<DisplayedTransactionDto>,
}

impl From<TransactionsUpdatedEvent> for TransactionsUpdatedDto {
    fn from(e: TransactionsUpdatedEvent) -> Self {
        Self {
            account_id: e.account_id,
            updated_transactions: e.updated_transactions.into_iter().map(Into::into).collect(),
        }
    }
}

/// Inputs to [`start_scan`] and [`rescan`].
///
/// `wallet_name` selects the account (it must be unlocked, and match the handle);
/// `base_url` is the base node RPC endpoint; `batch_size` is the number of blocks
/// fetched per request; `continuous` keeps following the tip after catching up;
/// `poll_interval_seconds` is the wait between polls in continuous mode;
/// `required_confirmations` is the confirmation depth in blocks; `priority` decides
/// what a second scan of the same wallet does.
///
/// Resource controls (`None` keeps minotari's default): `processing_threads` sizes the
/// output-detection pool, clamped to between 1 and the device's available parallelism; `event_buffer` bounds the event queue (the scan then waits
/// for a slow listener instead of buffering); `deadline_seconds` ends the scan with
/// `BudgetExhausted` once that long has passed; `max_blocks` ends a one-shot scan with
/// `BudgetExhausted` after that many blocks (ignored in continuous mode).
#[frb]
pub struct ScanConfiguration {
    pub wallet_name: String,
    pub base_url: String,
    pub batch_size: u64,
    pub continuous: bool,
    pub poll_interval_seconds: u64,
    pub required_confirmations: u64,
    pub priority: ScanPriority,
    pub processing_threads: Option<u32>,
    pub event_buffer: Option<u32>,
    pub deadline_seconds: Option<u64>,
    pub max_blocks: Option<u64>,
}

/// The event-forwarder loop: delivers every non-terminal event to `callback`, in
/// order, and returns the scan's terminal event for the caller to send last.
///
/// It reads until the scanner drops its sender, also after a cancel: with a bounded
/// `event_buffer` the scanner waits for room to send its terminal event, so the reader
/// must keep going for a cancelled scan to finish. A closed Dart stream (callback
/// `Err`) cancels the scan, and later events are read but not delivered. **Scans cancel
/// on a closed stream**; sends deliberately continue (see `send_transaction`).
async fn run_forwarder<F>(
    mut rx: EventReceiver,
    cancel_token: CancellationToken,
    callback: Arc<F>,
) -> Option<ScanEventDto>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    let mut terminal = None;
    let mut sink_open = true;
    while let Some(event) = rx.recv().await {
        let Some(dto) = map_processing_event(event) else {
            continue;
        };
        if is_terminal(&dto) {
            terminal = Some(dto);
            continue;
        }
        if sink_open && callback(dto).is_err() {
            sink_open = false;
            cancel_token.cancel();
        }
    }
    terminal
}

#[frb(ignore)]
pub async fn start_scan_with_handler<F>(
    handle: ScanHandle,
    config: ScanConfiguration,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    run_scan(handle, config, None, event_callback).await
}

#[frb(ignore)]
pub async fn rescan_with_handler<F>(
    handle: ScanHandle,
    from_height: u64,
    config: ScanConfiguration,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    run_scan(handle, config, Some(from_height), event_callback).await
}

/// The one scan implementation behind both scan APIs: runs the scan and sends its one
/// terminal event.
async fn run_scan<F>(
    handle: ScanHandle,
    config: ScanConfiguration,
    rescan_from: Option<u64>,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    // Fires when this function returns (after the terminal event is sent and the
    // callback dropped) or its future is dropped, so a stop waits for the stream to end.
    let stopped = CancellationToken::new();
    let _stopped_on_return = stopped.clone().drop_guard();
    let callback = Arc::new(event_callback);
    match claim_and_scan(handle, config, rescan_from, callback.clone(), stopped).await {
        Ok(terminal) => {
            let _ = callback(terminal);
            Ok(())
        }
        Err(e) => {
            let (message, error) = scan_failure(e);
            let _ = callback(ScanEventDto::Error(message));
            Err(error)
        }
    }
}

/// Claim `handle` for this scan, then run it. Returns the terminal event.
async fn claim_and_scan<F>(
    handle: ScanHandle,
    config: ScanConfiguration,
    rescan_from: Option<u64>,
    callback: Arc<F>,
    stopped: CancellationToken,
) -> Result<ScanEventDto>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    if handle.wallet_name != config.wallet_name {
        return Err(WalletError::wallet("The scan handle belongs to another wallet").into());
    }
    {
        let mut state = write_state()?;
        if state.stopped_before_start.remove(&handle.id) {
            return Ok(ScanEventDto::Cancelled);
        }
        if !state.issued.remove(&handle.id) {
            return Err(WalletError::wallet("This scan handle was already used").into());
        }
        state.starting.insert(handle.id, handle.wallet_name.clone());
    }
    let result = scan(handle.id, config, rescan_from, callback, stopped).await;
    if let Ok(mut state) = SCAN_STATE.write() {
        state.starting.remove(&handle.id);
    }
    result
}

/// Take the wallet's slot (waiting for the scan it replaces to stop), scan, and leave
/// the slot again.
async fn scan<F>(
    id: u64,
    config: ScanConfiguration,
    rescan_from: Option<u64>,
    callback: Arc<F>,
    stopped: CancellationToken,
) -> Result<ScanEventDto>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    let db_path = get_db_path()?;
    unlocked_account(&*get_db_connection()?, &config.wallet_name)?;
    // Refuse a bad rescan height before taking the slot, so it never replaces a scan.
    if let Some(from_height) = rescan_from {
        check_rescan_height(from_height)?;
    }

    let token = CancellationToken::new();
    let superseded = Arc::new(AtomicBool::new(false));

    // Wait out a maintenance operation before taking the slot. A stop issued
    // meanwhile removed this scan from `starting`.
    let maintenance = begin_maintenance(&config.wallet_name).await?;
    let previous = {
        let mut state = write_state()?;
        if state.starting.remove(&id).is_none() {
            return Ok(ScanEventDto::Cancelled);
        }
        take_slot(
            &mut state,
            &config.wallet_name,
            ScanController {
                id,
                priority: config.priority,
                token: token.clone(),
                superseded: superseded.clone(),
                stopped: stopped.clone(),
            },
        )?
    };
    drop(maintenance);
    if let Some(previous) = previous {
        previous.stopped.cancelled().await;
    }

    let outcome = scan_in_slot(&config, db_path, rescan_from, &token, &superseded, callback).await;
    clear_scan_if_current(&config.wallet_name, id);
    // The scanner reads the keys through the session with an empty passphrase; if the
    // wallet was locked meanwhile, its failure is reported as `Wallet is locked`.
    outcome.map_err(|e| {
        match get_db_connection().and_then(|conn| is_unlocked(&conn, &config.wallet_name)) {
            Ok(false) => WalletError::Locked.into(),
            _ => e,
        }
    })
}

/// Put `controller` in `wallet_name`'s slot by the priority rules, returning the scan
/// it replaced (already cancelled and, unless it was being stopped anyway, marked
/// superseded). A Background scan never replaces a running Foreground one.
fn take_slot(
    state: &mut ScanState,
    wallet_name: &str,
    controller: ScanController,
) -> Result<Option<ScanController>> {
    if let Some(current) = state.running.get(wallet_name) {
        if current.priority == ScanPriority::Foreground
            && controller.priority == ScanPriority::Background
            && !current.token.is_cancelled()
        {
            return Err(WalletError::ForegroundScanActive.into());
        }
    }
    let previous = state.running.insert(wallet_name.to_string(), controller);
    if let Some(previous) = &previous {
        if !previous.token.is_cancelled() {
            previous.superseded.store(true, Ordering::SeqCst);
        }
        previous.token.cancel();
    }
    Ok(previous)
}

/// The terminal event of a scan stopped before the scanner ran.
fn stopped_event(superseded: &AtomicBool) -> ScanEventDto {
    if superseded.load(Ordering::SeqCst) {
        ScanEventDto::Superseded
    } else {
        ScanEventDto::Cancelled
    }
}

async fn scan_in_slot<F>(
    config: &ScanConfiguration,
    db_path: PathBuf,
    rescan_from: Option<u64>,
    token: &CancellationToken,
    superseded: &AtomicBool,
    callback: Arc<F>,
) -> Result<ScanEventDto>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    // Stopped while waiting for the scan it replaced: leave the DB untouched.
    if token.is_cancelled() {
        return Ok(stopped_event(superseded));
    }
    // Best-effort: free inputs a stale reservation still holds, now that the previous
    // scan has stopped.
    match get_db_connection() {
        Ok(conn) => release_expired_reservations(&conn),
        Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
    }
    if let Some(from_height) = rescan_from {
        rollback_account(&config.wallet_name, from_height).await?;
        if token.is_cancelled() {
            return Ok(stopped_event(superseded));
        }
    }

    let (rx, scan_future) = build_scanner(config, db_path, token.clone()).run_with_events();
    let forwarder = tokio::spawn(run_forwarder(rx, token.clone(), callback));
    let result = scan_future.await;
    // The scanner dropped its sender, so the forwarder drains and ends.
    let terminal = match forwarder.await {
        Ok(terminal) => terminal,
        Err(join_err) => {
            log::error!("Scan event-forwarder task failed: {join_err}");
            None
        }
    };
    result.map_err(|e| anyhow!(e))?;
    if superseded.load(Ordering::SeqCst) {
        return Ok(ScanEventDto::Superseded);
    }
    Ok(terminal.unwrap_or(ScanEventDto::Cancelled))
}

fn build_scanner(
    config: &ScanConfiguration,
    db_path: PathBuf,
    token: CancellationToken,
) -> Scanner {
    let mode = if config.continuous {
        ScanMode::Continuous {
            poll_interval: Duration::from_secs(config.poll_interval_seconds),
        }
    } else {
        match config.max_blocks {
            Some(max_blocks) => ScanMode::Partial { max_blocks },
            None => ScanMode::Full,
        }
    };
    // An empty passphrase makes minotari use the wallet's unlocked key session.
    let mut scanner = Scanner::new(
        "",
        &config.base_url,
        db_path,
        config.batch_size,
        config.required_confirmations,
    )
    .mode(mode)
    .account(&config.wallet_name)
    .cancel_token(token);
    if let Some(threads) = config.processing_threads {
        scanner = scanner.processing_threads(capped_threads(threads));
    }
    if let Some(size) = config.event_buffer {
        scanner = scanner.event_buffer(Some(size as usize));
    }
    if let Some(seconds) = config.deadline_seconds {
        if let Some(deadline) = Instant::now().checked_add(Duration::from_secs(seconds)) {
            scanner = scanner.deadline(deadline);
        }
    }
    scanner
}

/// `requested` processing threads, between 1 and the device's available parallelism.
fn capped_threads(requested: u32) -> usize {
    let max = std::thread::available_parallelism().map_or(1, |n| n.get());
    (requested as usize).clamp(1, max)
}

/// The DB stores heights as i64; a larger rescan height would wrap and roll back
/// everything, so it is refused.
fn check_rescan_height(from_height: u64) -> Result<()> {
    if i64::try_from(from_height).is_err() {
        return Err(
            WalletError::database(format!("Rescan height {from_height} is out of range")).into(),
        );
    }
    Ok(())
}

/// Roll `wallet_name` back so its next scan starts at `from_height`, in one write
/// transaction.
async fn rollback_account(wallet_name: &str, from_height: u64) -> Result<()> {
    check_rescan_height(from_height)?;
    let mut conn = get_db_connection()?;
    let wallet_name = wallet_name.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let db_error = |e: r2d2_sqlite::rusqlite::Error| WalletError::database(e.to_string());
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let account = get_account_by_name(&tx, &wallet_name)
            .map_err(|e| WalletError::database(e.to_string()))?
            .ok_or(WalletError::NoAccounts)?;
        rollback_from_height(&tx, account.id, from_height, None)
            .map_err(|e| WalletError::database(format!("Rollback failed: {e}")))?;
        tx.commit().map_err(db_error)?;
        Ok(())
    })
    .await
    .map_err(|e| WalletError::internal(format!("Rollback task failed: {e}")))?
}

/// The terminal `Error` event text and the returned error for a failed scan.
///
/// Both can carry node-supplied words, so invisible characters are dropped. Dart
/// receives an error as its `{:?}` rendering (message plus "Caused by" chain), so the
/// returned error carries that whole rendering, cleaned, as its message.
fn scan_failure(e: anyhow::Error) -> (String, anyhow::Error) {
    let message = strip_format_chars(&e.to_string());
    let error = anyhow!(strip_format_chars(&format!("{e:?}")));
    (message, error)
}

/// Scan the blockchain for wallet `config.wallet_name`, streaming progress.
///
/// `handle` comes from [`create_scan_handle`] for the same wallet, which must be
/// unlocked (`Wallet is locked` otherwise). With `config.continuous` set it keeps
/// following the chain tip until stopped with [`stop_scan`]. A second scan of the same
/// wallet replaces this one (this stream then ends with `Superseded`), unless this one
/// is Foreground and the new one Background, which is refused with
/// `ForegroundScanActive`. Scans of different wallets run side by side.
///
/// Async and streamed. Every stream ends with exactly one terminal event (see
/// [`ScanEventDto`]). The scan **cancels when the Dart stream is closed** — this
/// deliberately differs from
/// [`send_transaction`](crate::api::send_transaction::send_transaction), which
/// continues.
#[frb]
pub async fn start_scan(
    sink: StreamSink<ScanEventDto>,
    handle: ScanHandle,
    config: ScanConfiguration,
) -> Result<()> {
    start_scan_with_handler(handle, config, move |event| {
        // Sink-closed is an internal signal for `run_forwarder` to cancel the scan; it
        // never reaches Dart.
        sink.add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

/// Roll the wallet back to `from_height` and scan again from there, streaming the same
/// events as [`start_scan`], with the same handle and slot rules.
///
/// A `from_height` above `i64::MAX` is refused (`Database Error: Rescan height … is
/// out of range`) before the slot is taken, so it never replaces a running scan. The
/// rollback happens once this scan owns the wallet's slot (any scan it replaced
/// has stopped) and is committed before scanning starts; if the scan then fails, the
/// wallet stays rolled back until a later scan covers the range.
#[frb]
pub async fn rescan(
    sink: StreamSink<ScanEventDto>,
    handle: ScanHandle,
    from_height: u64,
    config: ScanConfiguration,
) -> Result<()> {
    rescan_with_handler(handle, from_height, config, move |event| {
        sink.add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

#[cfg(test)]
mod tests {
    //! Scan slot and stream tests. The slot rules are driven directly with stand-in
    //! scans (no node needed); the full start path is driven up to the point where it
    //! would contact a node. Tests touching the process-global slots hold
    //! `GLOBAL_STATE_TEST_LOCK`.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::api::db::{lock_wallet, unlock_wallet, GLOBAL_STATE_TEST_LOCK};
    use minotari_wallet::db::create_account;
    use minotari_wallet::scan::{BlockProcessedEvent, FastSyncPhase};
    use std::sync::Mutex;
    use tari_common_types::seeds::cipher_seed::CipherSeed;
    use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
    use tokio::sync::mpsc;

    /// A stand-in running scan: stops (clears its slot, fires `stopped`) once its
    /// token is cancelled, as `scan` does on exit.
    struct StandIn {
        token: CancellationToken,
        superseded: Arc<AtomicBool>,
        stopped: CancellationToken,
    }

    fn stand_in(wallet: &str, id: u64, priority: ScanPriority) -> (ScanController, StandIn) {
        let s = StandIn {
            token: CancellationToken::new(),
            superseded: Arc::new(AtomicBool::new(false)),
            stopped: CancellationToken::new(),
        };
        let controller = ScanController {
            id,
            priority,
            token: s.token.clone(),
            superseded: s.superseded.clone(),
            stopped: s.stopped.clone(),
        };
        let (token, stopped, wallet) = (s.token.clone(), s.stopped.clone(), wallet.to_string());
        tokio::spawn(async move {
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            clear_scan_if_current(&wallet, id);
            stopped.cancel();
        });
        (controller, s)
    }

    fn install(wallet: &str, controller: ScanController) {
        let mut state = SCAN_STATE.write().unwrap();
        assert!(take_slot(&mut state, wallet, controller).unwrap().is_none());
    }

    fn running_id(wallet: &str) -> Option<u64> {
        SCAN_STATE.read().unwrap().running.get(wallet).map(|c| c.id)
    }

    fn handle(wallet: &str, id: u64) -> ScanHandle {
        ScanHandle {
            wallet_name: wallet.to_string(),
            id,
        }
    }

    #[tokio::test]
    async fn foreground_supersedes_background_and_background_is_refused_during_foreground() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (background, old) = stand_in("slot-prio", 1, ScanPriority::Background);
        install("slot-prio", background);

        let (foreground, new) = stand_in("slot-prio", 2, ScanPriority::Foreground);
        let replaced = take_slot(&mut SCAN_STATE.write().unwrap(), "slot-prio", foreground)
            .unwrap()
            .expect("the background scan is replaced");
        assert_eq!(replaced.id, 1);
        assert!(old.token.is_cancelled());
        assert!(
            old.superseded.load(Ordering::SeqCst),
            "it ends as Superseded"
        );
        assert!(matches!(
            stopped_event(&old.superseded),
            ScanEventDto::Superseded
        ));
        old.stopped.cancelled().await;
        assert_eq!(
            running_id("slot-prio"),
            Some(2),
            "its exit left the new scan alone"
        );

        let (background, refused) = stand_in("slot-prio", 3, ScanPriority::Background);
        let err = take_slot(&mut SCAN_STATE.write().unwrap(), "slot-prio", background)
            .err()
            .expect("a background start is refused");
        assert_eq!(
            err.to_string(),
            "A foreground scan of this wallet is already running"
        );
        assert!(
            !new.token.is_cancelled(),
            "the foreground scan keeps running"
        );
        assert_eq!(running_id("slot-prio"), Some(2));
        refused.token.cancel();

        stop_scan(handle("slot-prio", 2)).await.unwrap();
        assert!(new.stopped.is_cancelled());
        assert_eq!(running_id("slot-prio"), None);
    }

    #[tokio::test]
    async fn same_priority_replaces_the_old_scan() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (first, old) = stand_in("slot-same", 10, ScanPriority::Background);
        install("slot-same", first);
        let (second, new) = stand_in("slot-same", 11, ScanPriority::Background);
        let replaced = take_slot(&mut SCAN_STATE.write().unwrap(), "slot-same", second).unwrap();
        assert_eq!(replaced.map(|c| c.id), Some(10));
        assert!(old.superseded.load(Ordering::SeqCst));
        new.token.cancel();
        new.stopped.cancelled().await;
    }

    #[tokio::test]
    async fn a_stale_handle_does_not_stop_the_current_scan() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (current, scan) = stand_in("slot-stale", 21, ScanPriority::Foreground);
        install("slot-stale", current);

        stop_scan(handle("slot-stale", 20)).await.unwrap();
        assert!(
            !scan.token.is_cancelled(),
            "a superseded/finished handle does nothing"
        );
        assert_eq!(running_id("slot-stale"), Some(21));

        stop_scan(handle("slot-stale", 21)).await.unwrap();
        assert!(
            scan.stopped.is_cancelled(),
            "stop_scan waits for the scan to stop"
        );
        // Stopping it again is a no-op.
        stop_scan(handle("slot-stale", 21)).await.unwrap();
    }

    #[tokio::test]
    async fn two_wallets_scan_side_by_side_and_stopping_one_leaves_the_other() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (a, scan_a) = stand_in("slot-a", 30, ScanPriority::Foreground);
        let (b, scan_b) = stand_in("slot-b", 31, ScanPriority::Foreground);
        install("slot-a", a);
        install("slot-b", b);

        stop_scan(handle("slot-a", 30)).await.unwrap();
        assert!(scan_a.stopped.is_cancelled());
        assert!(!scan_b.token.is_cancelled(), "wallet b keeps scanning");
        assert_eq!(running_id("slot-b"), Some(31));

        // A wallet's own lock / maintenance stop leaves other wallets alone too.
        let (a, scan_a) = stand_in("slot-a", 32, ScanPriority::Foreground);
        install("slot-a", a);
        stop_wallet_scans("slot-a").await.unwrap();
        assert!(scan_a.stopped.is_cancelled());
        assert!(!scan_b.token.is_cancelled());

        stop_wallet_scans("slot-b").await.unwrap();
        assert!(scan_b.stopped.is_cancelled());
    }

    // --- the full start path -----------------------------------------------------

    const PASSPHRASE: &str = "pass";

    /// Record every event a scan streams.
    fn recorder() -> (
        Arc<Mutex<Vec<ScanEventDto>>>,
        impl Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        (events, move |event| {
            sink.lock().unwrap().push(event);
            Ok(())
        })
    }

    fn config(wallet: &str, priority: ScanPriority) -> ScanConfiguration {
        ScanConfiguration {
            wallet_name: wallet.to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            batch_size: 10,
            continuous: false,
            poll_interval_seconds: 1,
            required_confirmations: 3,
            priority,
            processing_threads: None,
            event_buffer: None,
            deadline_seconds: None,
            max_blocks: None,
        }
    }

    /// A database in `dir` with one seed wallet per name; returns its path.
    fn new_database(dir: &tempfile::TempDir, wallets: &[&str]) -> String {
        let path = dir.path().join("wallet.db");
        let pool = minotari_wallet::init_db(path.clone()).unwrap();
        let conn = pool.get().unwrap();
        for name in wallets {
            let wallet = WalletType::SeedWords(
                SeedWordsWallet::construct_new(CipherSeed::random()).unwrap(),
            );
            create_account(&conn, name, &wallet, PASSPHRASE).unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    /// One event that is the scan's terminal one.
    fn only_terminal(events: &Mutex<Vec<ScanEventDto>>) -> ScanEventDto {
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event: {events:?}");
        assert!(is_terminal(&events[0]));
        events[0].clone()
    }

    #[tokio::test]
    async fn locked_wallets_and_reused_handles_end_with_one_error() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["start-locked"]);
        crate::api::db::set_database_path(path).unwrap();

        let h = create_scan_handle("start-locked".into()).unwrap();
        let (events, callback) = recorder();
        let err = start_scan_with_handler(
            h.clone(),
            config("start-locked", ScanPriority::Foreground),
            callback,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "Wallet is locked");
        assert!(
            matches!(only_terminal(&events), ScanEventDto::Error(m) if m == "Wallet is locked")
        );

        let (events, callback) = recorder();
        let err = start_scan_with_handler(
            h,
            config("start-locked", ScanPriority::Foreground),
            callback,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Wallet Error: This scan handle was already used"
        );
        assert!(matches!(only_terminal(&events), ScanEventDto::Error(_)));

        let other = create_scan_handle("someone-else".into()).unwrap();
        let (_, callback) = recorder();
        let err = start_scan_with_handler(
            other,
            config("start-locked", ScanPriority::Foreground),
            callback,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("another wallet"), "{err}");
    }

    #[tokio::test]
    async fn a_handle_stopped_before_its_scan_starts_ends_cancelled() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let h = create_scan_handle("start-early-stop".into()).unwrap();
        stop_scan(h.clone()).await.unwrap();
        let (events, callback) = recorder();
        start_scan_with_handler(
            h,
            config("start-early-stop", ScanPriority::Foreground),
            callback,
        )
        .await
        .unwrap();
        assert!(matches!(only_terminal(&events), ScanEventDto::Cancelled));
    }

    /// An out-of-range rescan height is refused before the slot is taken, so it never
    /// replaces the running scan, even one it would supersede.
    #[tokio::test]
    async fn an_out_of_range_rescan_leaves_the_current_scan_alone() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let wallet = "rescan-range";
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &[wallet]);
        unlock_wallet(path, wallet.into(), PASSPHRASE.into()).unwrap();

        let (background, current) = stand_in(wallet, u64::MAX - 10, ScanPriority::Background);
        install(wallet, background);
        let h = create_scan_handle(wallet.into()).unwrap();
        let (events, callback) = recorder();
        let err = rescan_with_handler(
            h,
            u64::MAX,
            config(wallet, ScanPriority::Foreground),
            callback,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Database Error: Rescan height {} is out of range", u64::MAX)
        );
        assert!(matches!(only_terminal(&events), ScanEventDto::Error(_)));
        assert!(
            !current.token.is_cancelled(),
            "the running scan keeps going"
        );
        assert!(!current.superseded.load(Ordering::SeqCst));
        assert_eq!(
            running_id(wallet),
            Some(u64::MAX - 10),
            "its slot is untouched"
        );

        stop_wallet_scans(wallet).await.unwrap();
        lock_wallet(wallet.into()).await.unwrap();
    }

    /// Drives real rescans of an unlocked wallet. The node is unreachable, so a scan
    /// that got past the slot fails as soon as it contacts it (after the rollback).
    #[tokio::test]
    async fn start_path_slot_rules_and_maintenance() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let wallet = "start-unlocked";
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &[wallet]);
        unlock_wallet(path, wallet.into(), PASSPHRASE.into()).unwrap();
        let rescan = |priority| {
            let h = create_scan_handle(wallet.into()).unwrap();
            let (events, callback) = recorder();
            let task = tokio::spawn(rescan_with_handler(
                h.clone(),
                0,
                config(wallet, priority),
                callback,
            ));
            (h, events, task)
        };

        // A Foreground start supersedes a Background scan of the same wallet, after it
        // has stopped.
        let (background, old) = stand_in(wallet, u64::MAX - 1, ScanPriority::Background);
        install(wallet, background);
        let (_, events, task) = rescan(ScanPriority::Foreground);
        task.await.unwrap().expect_err("the node is unreachable");
        assert!(
            old.stopped.is_cancelled(),
            "the background scan stopped first"
        );
        assert!(matches!(
            stopped_event(&old.superseded),
            ScanEventDto::Superseded
        ));
        assert!(matches!(only_terminal(&events), ScanEventDto::Error(_)));
        assert_eq!(running_id(wallet), None, "the failed scan left its slot");

        // A Background start during a Foreground scan is refused.
        let (foreground, current) = stand_in(wallet, u64::MAX - 2, ScanPriority::Foreground);
        install(wallet, foreground);
        let (_, events, task) = rescan(ScanPriority::Background);
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(
            err.to_string(),
            "A foreground scan of this wallet is already running"
        );
        assert!(matches!(only_terminal(&events), ScanEventDto::Error(_)));
        assert!(!current.token.is_cancelled());
        stop_wallet_scans(wallet).await.unwrap();

        // A scan started during maintenance waits, without taking the slot...
        let maintenance = begin_maintenance(wallet).await.unwrap();
        let (_, _, task) = rescan(ScanPriority::Foreground);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished(), "the scan waits for the maintenance");
        assert_eq!(running_id(wallet), None, "nor takes the slot");
        drop(maintenance);
        task.await
            .unwrap()
            .expect_err("then it runs, and fails at the unreachable node");

        // ...and one stopped while it waits never runs.
        let maintenance = begin_maintenance(wallet).await.unwrap();
        let (_, events, task) = rescan(ScanPriority::Foreground);
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_wallet_scans(wallet).await.unwrap();
        drop(maintenance);
        task.await.unwrap().expect("a stopped scan resolves Ok");
        assert!(matches!(only_terminal(&events), ScanEventDto::Cancelled));
        assert_eq!(running_id(wallet), None);

        // Maintenance on another wallet (e.g. its passphrase change) does not delay a
        // scan of this one.
        let other = begin_maintenance("start-other-wallet").await.unwrap();
        let (_, _, task) = rescan(ScanPriority::Foreground);
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("not blocked by another wallet's maintenance")
            .unwrap()
            .expect_err("it runs, and fails at the unreachable node");
        drop(other);

        lock_wallet(wallet.into()).await.unwrap();
    }

    /// A failed unlock that takes the wallet's count to zero locks it like any lock,
    /// so the wallet's scans are cancelled too.
    #[tokio::test]
    async fn a_failed_unlock_reaching_zero_cancels_the_wallets_scans() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let wallet = "unlock-fail-cancels";
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &[wallet, "unlock-fail-other"]);
        let (running, scan) = stand_in(wallet, u64::MAX - 20, ScanPriority::Foreground);
        install(wallet, running);
        let (other_running, other) =
            stand_in("unlock-fail-other", u64::MAX - 21, ScanPriority::Foreground);
        install("unlock-fail-other", other_running);

        let err = unlock_wallet(path, wallet.into(), "wrong".into()).unwrap_err();
        assert_eq!(err.to_string(), "Invalid Passphrase");
        assert!(scan.token.is_cancelled(), "the wallet's scan is cancelled");
        assert!(!scan.superseded.load(Ordering::SeqCst), "it ends Cancelled");
        assert!(!other.token.is_cancelled(), "other wallets keep scanning");
        scan.stopped.cancelled().await;
        stop_wallet_scans("unlock-fail-other").await.unwrap();
    }

    /// A force lock runs while an unlock waits for the gate; the unlock then stores a
    /// session and a scan takes its slot before the unlock notices its count is gone.
    /// Locking that session also cancels the scan.
    #[tokio::test]
    async fn a_force_locked_unlock_cancels_a_scan_started_in_the_gap() {
        use crate::api::db::{confirm_still_counted_for_test, force_remove_entry};

        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let wallet = "force-gap-scan";
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &[wallet]);

        // The unlock stored its session; the force lock had removed its count.
        unlock_wallet(path, wallet.into(), PASSPHRASE.into()).unwrap();
        let generation = force_remove_entry(wallet).unwrap();
        // The other engine's scan saw the session and took a slot.
        let (running, scan) = stand_in(wallet, u64::MAX - 30, ScanPriority::Foreground);
        install(wallet, running);

        let err = confirm_still_counted_for_test(wallet, generation).unwrap_err();
        assert_eq!(err.to_string(), "Wallet is locked");
        assert!(
            scan.token.is_cancelled(),
            "the scan is cancelled with the lock"
        );
        assert!(!scan.superseded.load(Ordering::SeqCst), "it ends Cancelled");
        assert!(
            !minotari_wallet::session::is_unlocked(&get_db_connection().unwrap(), wallet).unwrap(),
            "the session is locked"
        );
        scan.stopped.cancelled().await;
        crate::api::db::disconnect_database().unwrap();
    }

    #[test]
    fn processing_threads_are_capped_at_the_available_parallelism() {
        let max = std::thread::available_parallelism().unwrap().get();
        assert_eq!(capped_threads(0), 1);
        assert_eq!(capped_threads(1), 1);
        assert_eq!(capped_threads(u32::MAX), max);
    }

    #[test]
    fn scan_failure_keeps_the_error_chain() {
        let (message, error) = scan_failure(anyhow!("bad\u{202E}node"));
        assert_eq!(message, "badnode");
        assert_eq!(format!("{error:?}"), "badnode");
        let chained = anyhow!("inner").context("outer");
        let (message, error) = scan_failure(chained);
        assert_eq!(message, "outer");
        assert!(format!("{error:?}").contains("Caused by"));
    }

    // --- run_forwarder -----------------------------------------------------------

    /// A fake event-callback recording every DTO it receives. `fail` flips it to
    /// returning `Err` (modelling a closed Dart `StreamSink`).
    struct FakeSink {
        received: Mutex<Vec<ScanEventDto>>,
        fail: bool,
    }

    impl FakeSink {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                received: Mutex::new(Vec::new()),
                fail,
            })
        }

        fn callback(
            self: &Arc<Self>,
        ) -> Arc<impl Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static> {
            let me = self.clone();
            Arc::new(move |event| {
                me.received.lock().unwrap().push(event);
                if me.fail {
                    Err(anyhow!("sink closed"))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn status(event: ScanStatusEvent) -> ProcessingEvent {
        ProcessingEvent::ScanStatus(event)
    }

    fn progress_event() -> ProcessingEvent {
        status(ScanStatusEvent::Progress {
            account_id: 1,
            current_height: 100,
            tip_height: 250,
            blocks_scanned: 10,
        })
    }

    fn completed_event() -> ProcessingEvent {
        status(ScanStatusEvent::Completed {
            account_id: 1,
            final_height: 110,
            total_blocks_scanned: 110,
        })
    }

    fn reorg_event() -> ProcessingEvent {
        ProcessingEvent::ReorgDetected(ReorgDetectedEvent {
            account_id: 1,
            reorg_from_height: 95,
            new_height: 97,
            blocks_rolled_back: 6,
            invalidated_output_hashes: Vec::new(),
            cancelled_transaction_ids: vec!["tx-1".to_string(), "tx-2".to_string()],
            reorganized_displayed_transactions: Vec::new(),
        })
    }

    /// Non-terminal events reach the sink in order; the terminal one is returned for
    /// the caller to send last.
    #[tokio::test]
    async fn forwarder_delivers_in_order_and_returns_the_terminal() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);
        let handle = tokio::spawn(run_forwarder(
            EventReceiver::Unbounded(rx),
            token.clone(),
            sink.callback(),
        ));

        tx.send(progress_event()).unwrap();
        tx.send(reorg_event()).unwrap();
        tx.send(ProcessingEvent::BlockProcessed(BlockProcessedEvent {
            account_id: 1,
            height: 105,
            block_hash: Vec::new(),
            outputs_detected: Vec::new(),
            inputs_spent: Vec::new(),
            balance_changes: Vec::new(),
        }))
        .unwrap();
        tx.send(completed_event()).unwrap();
        drop(tx);

        let terminal = handle.await.unwrap();
        assert!(matches!(
            terminal,
            Some(ScanEventDto::Completed {
                final_height: 110,
                ..
            })
        ));
        assert!(!token.is_cancelled());
        let got = sink.received.lock().unwrap();
        assert_eq!(
            got.len(),
            2,
            "BlockProcessed and the terminal are not delivered: {got:?}"
        );
        assert!(matches!(
            got[0],
            ScanEventDto::Progress {
                current_height: 100,
                tip_height: 250,
                ..
            }
        ));
        let ScanEventDto::ReorgDetected(reorg) = &got[1] else {
            panic!("expected a reorg, got {:?}", got[1]);
        };
        assert_eq!(reorg.reorg_from_height, 95);
        assert_eq!(reorg.cancelled_transaction_ids, vec!["tx-1", "tx-2"]);
    }

    /// A closed sink cancels the scan, but the forwarder keeps reading so a bounded
    /// scanner can still send its terminal event and finish.
    #[tokio::test]
    async fn forwarder_cancels_on_sink_closed_and_keeps_draining() {
        let (tx, rx) = mpsc::channel(1);
        let token = CancellationToken::new();
        let sink = FakeSink::new(true);
        let handle = tokio::spawn(run_forwarder(
            EventReceiver::Bounded(rx),
            token.clone(),
            sink.callback(),
        ));

        tx.send(progress_event()).await.unwrap();
        tx.send(progress_event()).await.unwrap();
        tx.send(status(ScanStatusEvent::Cancelled {
            account_id: 1,
            last_scanned_height: 100,
        }))
        .await
        .unwrap();
        drop(tx);

        let terminal = handle.await.unwrap();
        assert!(
            token.is_cancelled(),
            "sink-closed must cancel the scan token"
        );
        assert!(matches!(terminal, Some(ScanEventDto::Cancelled)));
        assert_eq!(
            sink.received.lock().unwrap().len(),
            1,
            "nothing after the failed delivery"
        );
    }

    #[test]
    fn terminal_and_fast_sync_events_map_as_documented() {
        assert!(
            map_processing_event(status(ScanStatusEvent::FastSyncPhaseStarted {
                account_id: 1,
                phase: FastSyncPhase::FastUtxoScan,
                from_height: 100,
                to_height: Some(200),
            }))
            .is_none()
        );
        assert!(
            map_processing_event(status(ScanStatusEvent::FastSyncPhaseCompleted {
                account_id: 1,
                phase: FastSyncPhase::FastUtxoScan,
            }))
            .is_none()
        );
        assert!(matches!(
            map_processing_event(status(ScanStatusEvent::BudgetExhausted {
                account_id: 1,
                last_scanned_height: 5,
                tip_height: 9,
            })),
            Some(ScanEventDto::BudgetExhausted {
                last_scanned_height: 5,
                tip_height: 9,
                ..
            })
        ));
        assert!(!is_terminal(&ScanEventDto::Waiting {
            account_id: 1,
            resume_in_seconds: 1
        }));
        assert!(is_terminal(&ScanEventDto::Superseded));
    }
}
