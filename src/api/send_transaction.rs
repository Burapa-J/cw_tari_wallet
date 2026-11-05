use crate::api::config::{
    DEFAULT_BASE_URL, DEFAULT_CONFIRMATION_WINDOW, SECONDS_TO_LOCK_UTXO, SEND_FEE_PER_GRAM,
};
use crate::api::db::{
    blocking, get_db_connection, get_db_pool, is_unlocked, release_expired_reservations,
    session_gone, unlocked_account, with_session,
};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::scanner::{begin_maintenance, stop_wallet_scans};
use crate::api::transactions::DisplayedTransactionDto;
use crate::domain::text::strip_format_chars;
use crate::domain::validation::{validate_recipient, validate_recipient_count};
use crate::frb_generated::StreamSink;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::db::{expire_and_unlock_pending_transaction, WalletDbError};
use minotari_wallet::get_accounts;
use minotari_wallet::transactions::abandon::AbandonSendError;
use minotari_wallet::transactions::manager::{FinalizedSend, TransactionSender};
use minotari_wallet::transactions::one_sided_transaction::Recipient;
use tari_common::configuration::Network;
use tari_common_types::transaction::TxId;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::wallet_types::WalletType;
use tari_transaction_components::key_manager::KeyManager;
use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
use tari_transaction_components::offline_signing::{
    present_payload_summary, sign_locked_transaction, ConfirmationMode, ConfirmedPayload,
    PayloadSummary,
};
use tari_transaction_components::transaction_builder::TransactionBuilderError;
use tari_transaction_components::MicroMinotari;
use zeroize::Zeroizing;

/// One payee of a [`send_transaction`].
///
/// `address` is the recipient's base58 Tari address; `amount` is in **microTari**
/// (ignored for a send-all); `payment_id` is an optional memo for this recipient.
#[frb]
pub struct SendRecipient {
    pub address: String,
    pub amount: u64,
    pub payment_id: Option<String>,
}

/// Inputs to [`send_transaction`].
///
/// `wallet_name` selects the account, which signs with its unlocked key session
/// (`unlock_wallet`); a locked wallet errors as `Wallet is locked` and a view-only
/// wallet as `Signing Error: View-only wallets cannot send`. `network` selects the
/// Tari network (`None` → MainNet); `base_url` is the base node RPC endpoint (`None` →
/// the default mainnet RPC); every entry of `recipients` gets one output, amounts in
/// **microTari**; `confirmation_window` is the required confirmations in blocks
/// (`None` → 3). With `send_all` set, `recipients` must hold exactly one entry and it
/// receives every spendable output less the fee; quote that first with
/// [`estimate_send_all_fee`](crate::api::fee::estimate_send_all_fee).
#[frb]
pub struct MultiSendTransactionDetails {
    pub network: Option<TariNetwork>,
    pub base_url: Option<String>,
    pub wallet_name: String,
    pub recipients: Vec<SendRecipient>,
    pub send_all: bool,
    pub confirmation_window: Option<u64>,
}

/// Validated, typed inputs for building a transaction.
struct ValidatedInputs {
    network: Network,
    recipients: Vec<Recipient>,
    confirmations: u64,
}

/// The lifecycle stage of an in-flight send, carried on each
/// [`SendTransactionEvent`]. Emitted in order from `Initializing` to `Broadcasted`.
///
/// `Broadcasted` means the send is recorded and was handed to the node, not that it
/// is confirmed: confirmation is reported by the scanner (`TransactionsUpdated`), not
/// by the send stream.
#[frb]
#[derive(Clone, Debug)]
pub enum TransactionStage {
    Initializing,
    ValidatingInput,
    ConnectingToNetwork,
    ConstructingTransaction,
    SigningKeyGeneration,
    SigningTransaction,
    Broadcasting,
    Broadcasted,
}

/// A progress event streamed during [`send_transaction`]: the current
/// [`TransactionStage`] plus a human-readable `details` message.
#[frb]
#[derive(Clone)]
pub struct SendTransactionEvent {
    pub stage: TransactionStage,
    pub details: String,
}

/// The outcome of a [`send_transaction`].
///
/// `transaction` is the recorded send, pending until the scan sees it mined.
/// `accepted_by_node` is whether the node took the first broadcast (or already had
/// it mined). When `false` the send is still recorded and the wallet rebroadcasts it:
/// **do not send it again**. `note` is the message to show the user in that case (or
/// when the wallet could not record everything yet); `node_reason` is the node's
/// answer when it did not accept the send (node-supplied text: display it as plain
/// text). Keep scanning so the wallet rebroadcasts it; if the node keeps refusing it
/// until it stalls, [`abandon_send`] gives it up and releases its inputs; see there
/// before paying the same payee again.
#[frb]
#[derive(Debug, Clone)]
pub struct SendResultDto {
    pub transaction: DisplayedTransactionDto,
    pub accepted_by_node: bool,
    pub note: Option<String>,
    pub node_reason: Option<String>,
}

impl From<FinalizedSend> for SendResultDto {
    fn from(sent: FinalizedSend) -> Self {
        Self {
            transaction: sent.displayed_transaction.into(),
            accepted_by_node: sent.accepted_by_node,
            note: sent.note,
            node_reason: sent.node_reason.map(|r| strip_format_chars(&r)),
        }
    }
}

/// The Dart-visible reason an `abandon_send` of a send that has not stalled is refused.
const NOT_STALLED: &str =
    "The send has not stalled yet; keep scanning so the wallet rebroadcasts it";

/// Give up a stalled send and release its inputs.
///
/// `transaction_id` is the send's `DisplayedTransactionDto.id`; `passphrase`
/// (**secret**) must open wallet `wallet_name`. Delegates to minotari, which only
/// abandons a send that is not mined and has stalled: the node refused its last
/// rebroadcasts in a row (rebroadcasts happen while a scan runs, so keep scanning
/// first). A mined send never stalls. A send without a change output is always
/// refused. Returns what abandoning does not do.
///
/// The wallet's running scan is stopped (and waited for) first, and a scan started
/// meanwhile waits until the abandon is done, so no rebroadcast races it; start the
/// scan again afterwards. Other wallets' scans keep running.
///
/// The original can still be mined if a node holds it. Right after an abandon,
/// `Rejected` means "not known to be mined", not "will not be mined": do not treat it
/// as proof until later scans from a trusted node still show it rejected. If you pay
/// the payee again, prefer a send that spends the same inputs, so it conflicts with
/// the original instead of paying twice. minotari selects the largest outputs first,
/// so a same-amount resend usually picks the released inputs again.
///
/// Errors: `Invalid Passphrase`, `No accounts found for this wallet`,
/// `Wallet Error: Invalid transaction id`, `Wallet Error: The send has not stalled
/// yet; keep scanning so the wallet rebroadcasts it`, `Wallet Error: The wallet was
/// renamed or replaced; try again` (the name changed hands before the abandon
/// started; nothing is abandoned), and `Wallet Error: …` with
/// minotari's reason (unknown send, already mined, input already spent, change in use
/// or already seen on chain, no change output).
#[frb]
pub async fn abandon_send(
    wallet_name: String,
    passphrase: String,
    transaction_id: String,
) -> Result<String> {
    let passphrase = Zeroizing::new(passphrase);
    let tx_id: u64 = transaction_id
        .parse()
        .map_err(|_| WalletError::wallet("Invalid transaction id"))?;

    // Check the passphrase (Argon2, off the runtime) before touching a running scan.
    let conn = get_db_connection()?;
    let name = wallet_name.clone();
    let account_id = blocking(move || {
        let accounts = get_accounts(&conn, Some(&name))?;
        let account = accounts.first().ok_or(WalletError::NoAccounts)?;
        account
            .verify_passphrase(&passphrase)
            .map_err(|e| match e {
                WalletDbError::DecryptionFailed(_) => WalletError::InvalidPassphrase,
                other => WalletError::wallet(other.to_string()),
            })?;
        Ok(account.id)
    })
    .await?;

    // Rebroadcasts only happen inside a scan; stop it, and keep new scans out, so
    // none races the abandon.
    let _maintenance = begin_maintenance(&wallet_name).await?;
    // The name may have been renamed, deleted or re-created before maintenance began.
    same_account(&*get_db_connection()?, &wallet_name, account_id)?;
    stop_wallet_scans(&wallet_name).await?;

    let mut conn = get_db_connection()?;
    // Never `force` (a send reset by a rescan or reorg can look unmined) and never
    // accept a send without a change output. A write transaction: off the runtime.
    let abandoned = blocking(move || {
        minotari_wallet::transactions::abandon::abandon_send(
            &mut conn,
            account_id,
            TxId::from(tx_id),
            false,
            false,
            None,
        )
        .map_err(|e| match e {
            // Upstream's text suggests a `force` flag this API does not offer.
            AbandonSendError::NotStalled { .. } => WalletError::wallet(NOT_STALLED).into(),
            other => WalletError::wallet(other.to_string()).into(),
        })
    })
    .await?;
    Ok(abandoned.note().to_string())
}

/// Resolves `Ok` even when the node has not accepted the send yet: it is recorded and
/// rebroadcast, so it must not be sent again.
#[frb(ignore)]
pub async fn send_transaction_with_handler<F>(
    mut details: MultiSendTransactionDetails,
    status_callback: F,
) -> Result<SendResultDto>
where
    F: Fn(SendTransactionEvent) + Send + Sync + 'static,
{
    let report = |stage: TransactionStage, msg: &str| {
        status_callback(SendTransactionEvent {
            stage,
            details: msg.to_string(),
        });
    };

    report(TransactionStage::Initializing, "Starting workflow...");

    report(TransactionStage::ValidatingInput, "Parsing inputs...");
    let validated = validate_inputs(&details)?;

    apply_network(validated.network)?;

    report(
        TransactionStage::ConnectingToNetwork,
        "Accessing wallet database...",
    );
    let mut sender = create_transaction_sender(
        &details.wallet_name,
        validated.network,
        validated.confirmations,
    )?;

    // Load the signing keys from the session *before* the transaction reserves any
    // UTXO, so a locked or view-only wallet costs nothing. No stage event is emitted
    // here; `SigningKeyGeneration` stays at its position below.
    let key_manager = stored_key_manager(&details.wallet_name)?;

    // Best-effort: free inputs a stale reservation still holds before selecting new ones.
    match get_db_connection() {
        Ok(conn) => release_expired_reservations(&conn),
        Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
    }

    report(
        TransactionStage::ConstructingTransaction,
        "Building transaction UTXOs...",
    );
    // From here on UTXOs are reserved. Upstream releases them on most of its failure
    // paths; the bridge releases on a signing failure and again as a backstop on a
    // finalize failure, which does nothing once the send is recorded.
    let (pending_transaction_id, unsigned_tx) = build_while_unlocked(
        &mut sender,
        &details.wallet_name,
        validated.recipients,
        details.send_all,
    )?;

    report(
        TransactionStage::SigningKeyGeneration,
        "Loading signing keys...",
    );

    report(
        TransactionStage::SigningTransaction,
        "Signing transaction...",
    );

    let consensus_constants = ConsensusConstantsBuilder::new(validated.network).build();

    let signed = confirm_own_payload(unsigned_tx).and_then(|confirmed| {
        sign_locked_transaction(
            &key_manager,
            consensus_constants,
            validated.network,
            confirmed,
        )
        .map_err(map_signing_error)
    });
    let signed_transaction = match signed {
        Ok(tx) => tx,
        Err(e) => {
            release_send_reservation(&pending_transaction_id);
            return Err(e.into());
        }
    };

    // Finalize reads the keys again (change output) through the session. Never record
    // or broadcast unless the wallet is seen unlocked, in the send's own database.
    if !unlocked_in(&sender.db_pool, &details.wallet_name) {
        release_send_reservation(&pending_transaction_id);
        return Err(WalletError::Locked.into());
    }

    report(TransactionStage::Broadcasting, "Broadcasting to network...");

    let base_url = details
        .base_url
        .take()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let sent = match sender
        .finalize_transaction_and_broadcast(signed_transaction, base_url)
        .await
    {
        Ok(sent) => sent,
        Err(e) => {
            release_send_reservation(&pending_transaction_id);
            // A lock during finalize makes its key read fail with a decryption error.
            if session_gone(&details.wallet_name) {
                return Err(WalletError::Locked.into());
            }
            return Err(WalletError::network(strip_format_chars(&e.to_string())).into());
        }
    };
    // The send is recorded either way; a send the node has not accepted is
    // rebroadcast by the scan's transaction monitor and must not be retried.
    if !sent.accepted_by_node {
        log::warn!(
            "Send recorded but not yet accepted by the node: note={:?}, node_reason={:?}",
            sent.note,
            sent.node_reason
        );
    }

    report(TransactionStage::Broadcasted, "Transaction broadcast");

    Ok(sent.into())
}

/// Approve this process's own payload for signing, without a prompt.
///
/// The payload was built from this wallet's database just before signing, so there is
/// no one to show the summary to; upstream logs it.
fn confirm_own_payload(
    unsigned_tx: PrepareOneSidedTransactionForSigningResult,
) -> Result<ConfirmedPayload<PrepareOneSidedTransactionForSigningResult>, WalletError> {
    let summary = PayloadSummary::from_one_sided(unsigned_tx);
    let confirmation = present_payload_summary(
        &summary,
        ConfirmationMode::NonInteractive,
        &mut std::io::sink(),
        &mut std::io::empty(),
    )
    .map_err(|e| WalletError::signing(e.to_string()))?;
    summary
        .confirm(confirmation)
        .map_err(|e| WalletError::signing(e.to_string()))
}

/// Release the UTXOs this send reserved, best-effort.
///
/// `start_new_transaction` flips the selected outputs to `Locked` and commits. The only
/// other release in this embedding is the expiry sweep (`release_expired_reservations`,
/// run at database init and at the start of each send and scan), which waits out
/// `SECONDS_TO_LOCK_UTXO`; this call hands them back straight away.
///
/// `pending_transaction_id` is the `pending_transactions` row id (not the idempotency
/// key). `expire_and_unlock_pending_transaction` only acts while that row is still
/// `Pending`, so it cannot hand back the inputs of a transaction that was recorded.
///
/// Best-effort by design: this runs on an error path that already has a cause worth
/// reporting, so a failure here is logged rather than allowed to mask the original error.
fn release_send_reservation(pending_transaction_id: &str) {
    let released = get_db_connection()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            expire_and_unlock_pending_transaction(&conn, pending_transaction_id)
                .map_err(|e| e.to_string())
        });

    match released {
        Ok(true) => log::info!("Released the UTXO reservation for a failed send"),
        // Already `Completed`/`Expired`: nothing to release, which is the safe outcome.
        Ok(false) => log::debug!("No pending UTXO reservation to release for a failed send"),
        Err(e) => log::warn!(
            "Failed to release the UTXO reservation for a failed send; these outputs stay \
             locked until the reservation expires and the next send, scan or init sweeps it: {e}"
        ),
    }
}

/// The wallet's key manager from its unlocked session.
fn stored_key_manager(wallet_name: &str) -> Result<KeyManager> {
    let conn = get_db_connection()?;
    let account = unlocked_account(&conn, wallet_name)?;
    // An empty passphrase makes minotari use the session.
    let wallet = with_session(&conn, wallet_name, || {
        account
            .decrypt_wallet_type("")
            .map_err(|e| WalletError::wallet(e.to_string()).into())
    })?;
    // Refuse before any UTXO is reserved: a view key cannot sign.
    if matches!(wallet, WalletType::ViewWallet(_)) {
        return Err(WalletError::signing("View-only wallets cannot send").into());
    }
    KeyManager::new(wallet).map_err(|e| WalletError::wallet(e.to_string()).into())
}

/// Convert an upstream signing failure into a [`WalletError::Signing`].
fn map_signing_error(e: TransactionBuilderError) -> WalletError {
    WalletError::signing(e.to_string())
}

/// Pay one or more recipients in one transaction, or sweep the wallet to one
/// recipient (`details.send_all`), streaming progress.
///
/// Streams a [`SendTransactionEvent`] for each [`TransactionStage`] over `sink` and
/// resolves to a [`SendResultDto`]: the recorded, `Pending` transaction plus whether
/// the node accepted it. The wallet must be unlocked; no seed words or passphrase are
/// needed. Each recipient gets its own output carrying its own `payment_id`.
///
/// `Ok` means the send is recorded, not necessarily accepted by the node; the scan's
/// transaction monitor rebroadcasts it while a scan runs, so **never resend after
/// `Ok`**. Confirmation is reported by the scanner (`TransactionsUpdated`), not by
/// this stream. `Network Error` only comes from failures before the send is
/// recorded. Once a send has stalled, [`abandon_send`] releases its inputs (see there
/// before paying the same payee again).
///
/// Async and streamed. The send **continues even if the Dart stream is closed**
/// (a half-broadcast transaction must finish) — this deliberately differs from
/// [`start_scan`](crate::api::scanner::start_scan), which cancels on a closed
/// sink. Errors propagate as the resolved `Err`.
#[frb]
pub async fn send_transaction(
    sink: StreamSink<SendTransactionEvent>,
    details: MultiSendTransactionDetails,
) -> Result<SendResultDto> {
    send_transaction_with_handler(details, move |event| {
        // A closed sink never aborts a send: aborting a half-broadcast transaction
        // could lose funds. Scans, by contrast, cancel (see `scanner::run_forwarder`).
        let _ = sink.add(event);
    })
    .await
}

/// Thin adapter: resolve the network (frozen `None → MainNet` default), validate each
/// recipient with the pure domain validator and apply the confirmation-window default.
/// `WalletError` is converted to the boundary `anyhow::Error` via `?`.
fn validate_inputs(details: &MultiSendTransactionDetails) -> Result<ValidatedInputs> {
    let network = parse_network(details.network);
    validate_recipient_count(details.recipients.len(), details.send_all)?;

    let mut recipients = Vec::with_capacity(details.recipients.len());
    for r in &details.recipients {
        let address = validate_recipient(network, &r.address, r.amount, details.send_all)?;
        recipients.push(Recipient {
            address,
            amount: MicroMinotari(r.amount),
            payment_id: r.payment_id.clone(),
        });
    }

    Ok(ValidatedInputs {
        network,
        recipients,
        confirmations: details
            .confirmation_window
            .unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
    })
}

/// Build the [`TransactionSender`] for `wallet_name`, which uses its key session.
fn create_transaction_sender(
    wallet_name: &str,
    network: Network,
    confirmations: u64,
) -> Result<TransactionSender> {
    let db_pool = get_db_pool().map_err(|e| WalletError::database(e.to_string()))?;

    let mut sender = TransactionSender::new(
        db_pool,
        wallet_name.to_string(),
        Zeroizing::new(String::new()),
        network,
        confirmations,
    )
    .map_err(|e| WalletError::wallet(e.to_string()))?;
    sender.fee_per_gram = MicroMinotari(SEND_FEE_PER_GRAM);
    Ok(sender)
}

/// Reserve inputs and build the unsigned transaction.
///
/// Returns the id of the pending row that now holds the `Locked` UTXOs alongside the
/// payload; the caller needs it to release them if a later step fails (see
/// [`release_send_reservation`]).
/// [`build_unsigned_transaction`], reporting `Wallet is locked` if the wallet was locked
/// meanwhile. The build reads the keys through the session with an empty passphrase;
/// after a lock it fails with a decryption error, or, for a wallet whose passphrase is
/// empty, succeeds through minotari's fallback. In that case the reservation is
/// released and nothing is signed.
fn build_while_unlocked(
    sender: &mut TransactionSender,
    wallet_name: &str,
    recipients: Vec<Recipient>,
    send_all: bool,
) -> Result<(String, PrepareOneSidedTransactionForSigningResult)> {
    let (pending_transaction_id, unsigned_tx) =
        build_unsigned_transaction(sender, recipients, send_all).map_err(|e| {
            if session_gone(wallet_name) {
                WalletError::Locked.into()
            } else {
                e
            }
        })?;
    if !unlocked_in(&sender.db_pool, wallet_name) {
        release_send_reservation(&pending_transaction_id);
        return Err(WalletError::Locked.into());
    }
    Ok((pending_transaction_id, unsigned_tx))
}

/// The send's gate: `true` only if `wallet_name` is known to be unlocked, checked on
/// the send's own pool (not the global path, which the other engine could change).
/// Any error reads as locked, so the gate fails closed.
fn unlocked_in(pool: &minotari_wallet::db::SqlitePool, wallet_name: &str) -> bool {
    pool.get()
        .ok()
        .is_some_and(|conn| matches!(is_unlocked(&conn, wallet_name), Ok(true)))
}

/// Whether `wallet_name` is still the account with `account_id`: a rename or delete
/// (and a re-create under the same name) between reading the id and acting on it is
/// refused.
fn same_account(
    conn: &r2d2_sqlite::rusqlite::Connection,
    wallet_name: &str,
    account_id: i64,
) -> Result<()> {
    let accounts = get_accounts(conn, Some(wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;
    if account.id != account_id {
        return Err(WalletError::wallet(WALLET_REPLACED).into());
    }
    Ok(())
}

/// The Dart-visible reason an abandon is refused because its wallet changed meanwhile.
const WALLET_REPLACED: &str = "The wallet was renamed or replaced; try again";

fn build_unsigned_transaction(
    sender: &mut TransactionSender,
    mut recipients: Vec<Recipient>,
    send_all: bool,
) -> Result<(String, PrepareOneSidedTransactionForSigningResult)> {
    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let tx = if send_all {
        // `validate_recipient_count` guarantees exactly one recipient here.
        let recipient = recipients
            .pop()
            .ok_or_else(|| WalletError::wallet("A send-all pays exactly one recipient"))?;
        sender.start_new_send_all_transaction(
            idempotency_key,
            recipient.address,
            recipient.payment_id,
            SECONDS_TO_LOCK_UTXO,
        )
    } else {
        sender.start_new_transaction_multi(idempotency_key, recipients, SECONDS_TO_LOCK_UTXO)
    }
    .map_err(|e| WalletError::wallet(format!("Failed to build transaction: {}", e)))?;

    Ok((sender.processed_transactions.id().to_string(), tx))
}

#[cfg(test)]
mod tests {
    //! Boundary-adapter tests for `validate_inputs`. The pure validation logic now
    //! lives in `domain::validation` (tested there as `WalletError` variants); these
    //! tests pin the *boundary* behavior the adapter is responsible for: resolving
    //! the network via `parse_network` and converting the domain `WalletError` into
    //! the exact frozen Dart-visible `anyhow::Error` string. The known-good
    //! recipient address is derived deterministically from fixed key bytes (NOT a
    //! real funded address), so the suite stays hermetic and reproducible.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::tari_address::TariAddress;
    use tari_common_types::types::CompressedPublicKey;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

    /// Build a deterministic, valid base58 Tari (dual) address from fixed key
    /// bytes. The bytes are small little-endian scalars (well below the group
    /// order) so `RistrettoSecretKey::from_canonical_bytes` always succeeds. This
    /// is obviously not a real address — it exists only to exercise the base58
    /// parse path in `validate_inputs`.
    fn deterministic_recipient_base58(network: Network) -> String {
        let mut view_bytes = [0u8; 32];
        view_bytes[0] = 7;
        let mut spend_bytes = [0u8; 32];
        spend_bytes[0] = 11;

        let view_sk = RistrettoSecretKey::from_canonical_bytes(&view_bytes)
            .expect("fixed view-key bytes must be a canonical scalar");
        let spend_sk = RistrettoSecretKey::from_canonical_bytes(&spend_bytes)
            .expect("fixed spend-key bytes must be a canonical scalar");

        let view_pk = CompressedPublicKey::from_secret_key(&view_sk);
        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);

        TariAddress::new_dual_address_with_default_features(view_pk, spend_pk, network)
            .expect("constructing a dual address from valid keys must succeed")
            .to_base58()
    }

    fn details_with(
        recipient: String,
        amount: u64,
        confirmation_window: Option<u64>,
    ) -> MultiSendTransactionDetails {
        MultiSendTransactionDetails {
            network: Some(TariNetwork::MainNet),
            base_url: None,
            wallet_name: "test-wallet".to_string(),
            recipients: vec![SendRecipient {
                address: recipient,
                amount,
                payment_id: None,
            }],
            send_all: false,
            confirmation_window,
        }
    }

    #[test]
    fn rejects_zero_amount_with_frozen_boundary_string() {
        let details = details_with(deterministic_recipient_base58(Network::MainNet), 0, None);
        // `ValidatedInputs` deliberately has no `Debug`, so we destructure the
        // `Result` instead of using `expect_err`.
        let Err(err) = validate_inputs(&details) else {
            panic!("zero amount must be rejected");
        };
        // BASELINE CONTRACT: this exact Dart-visible string predates WalletError
        // (legacy `TransactionError::WalletError("Amount must be greater than zero")`).
        // The domain returns `WalletError::Wallet`; the adapter `?` converts it to
        // this anyhow string. Cake Wallet may match on it; it must not change.
        assert_eq!(
            err.to_string(),
            "Wallet Error: Amount must be greater than zero"
        );
    }

    #[test]
    fn rejects_malformed_recipient_address_with_frozen_boundary_prefix() {
        let details = details_with("not-a-valid-base58-address".to_string(), 1_000, None);
        let Err(err) = validate_inputs(&details) else {
            panic!("bad address must be rejected");
        };
        // BASELINE CONTRACT: the boundary keeps the legacy
        // `"Invalid Recipient Address: …"` prefix (was `TransactionError::InvalidAddress`).
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn rejects_wrong_network_recipient_with_frozen_boundary_prefix() {
        // The wallet's details resolve to MainNet (`details_with` hard-codes it), but
        // the recipient address is encoded for Esmeralda — a cross-network send.
        let details = details_with(
            deterministic_recipient_base58(Network::Esmeralda),
            1_000,
            None,
        );
        let Err(err) = validate_inputs(&details) else {
            panic!("a cross-network recipient must be rejected");
        };
        // BASELINE CONTRACT: the new rejection reuses the frozen
        // `"Invalid Recipient Address: "` prefix (Shared Contracts §2).
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "unexpected message: {err}"
        );
        // Pin the exact new message. `Network`'s `Display` emits lowercase key strings
        // (`mainnet` / `esmeralda`), so the values are lowercase (ledger D2).
        assert_eq!(
            err.to_string(),
            "Invalid Recipient Address: address network (esmeralda) does not match the configured network (mainnet)"
        );
    }

    /// SEND-continues-on-sink-closed (contrast with SCAN-cancels). The send-side
    /// status callback has signature `Fn(SendTransactionEvent)` returning `()`, so a
    /// closed sink *structurally cannot* signal failure back into the transaction
    /// flow — the tx proceeds regardless. This pins that the wrapper closure
    /// `send_transaction` installs (a failing `sink.add` swallowed by `let _ = …`)
    /// keeps reporting every subsequent stage rather than aborting. We model it
    /// hermetically (no DB) by replaying the closure shape against a fake sink whose
    /// `add` always errors, and asserting all stages were still delivered in order.
    #[test]
    fn send_continues_reporting_when_sink_is_closed() {
        use std::sync::{Arc, Mutex};

        // A fake sink that always rejects (models a closed Dart StreamSink).
        let delivered: Arc<Mutex<Vec<TransactionStage>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_add = {
            let delivered = delivered.clone();
            move |event: SendTransactionEvent| -> Result<()> {
                delivered.lock().unwrap().push(event.stage);
                Err(WalletError::scan("sink closed").into())
            }
        };

        // The exact wrapper shape `send_transaction` uses: swallow the add error so
        // the status_callback (and therefore the surrounding tx flow) keeps going.
        let status_callback = move |event: SendTransactionEvent| {
            let _ = sink_add(event);
        };

        // Drive the full stage sequence through the callback as the real flow would.
        for stage in [
            TransactionStage::Initializing,
            TransactionStage::ValidatingInput,
            TransactionStage::Broadcasting,
            TransactionStage::Broadcasted,
        ] {
            status_callback(SendTransactionEvent {
                stage,
                details: String::new(),
            });
        }

        // Despite every `add` failing, all stages were still reported in order: the
        // send path does not cancel on sink-closed.
        let got = delivered.lock().unwrap();
        assert_eq!(got.len(), 4, "all stages reported even with a closed sink");
        assert!(matches!(got[0], TransactionStage::Initializing));
        assert!(matches!(got[3], TransactionStage::Broadcasted));
    }

    /// Sending needs the wallet's session: a locked wallet is refused before any UTXO
    /// is reserved, and an unlocked one signs and builds with no seed words or
    /// passphrase.
    #[tokio::test]
    async fn sending_needs_an_unlocked_session_and_no_seed_words() {
        use crate::api::db::{
            lock_wallet, set_database_path, unlock_wallet, GLOBAL_STATE_TEST_LOCK,
        };

        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (dir, pool, _, _) = reserve_one_output();
        drop(pool);
        let path = dir.path().join("wallet.db").to_string_lossy().into_owned();
        set_database_path(path.clone()).unwrap();

        let err = stored_key_manager("test")
            .err()
            .expect("a locked wallet cannot send");
        assert_eq!(err.to_string(), "Wallet is locked");

        unlock_wallet(path, "test".into(), "pass".into()).unwrap();
        stored_key_manager("test").expect("the session signs");
        let mut sender = create_transaction_sender("test", Network::LocalNet, 3).unwrap();
        let conn = get_db_connection().unwrap();
        let address = unlocked_account(&conn, "test")
            .unwrap()
            .get_address(Network::LocalNet, "")
            .unwrap();
        // The output the fixture reserved is still locked, so free it first.
        conn.execute(
            "UPDATE pending_transactions SET expires_at = '2000-01-01 00:00:00'",
            [],
        )
        .unwrap();
        release_expired_reservations(&conn);
        let recipient = Recipient {
            address,
            amount: MicroMinotari(1_000),
            payment_id: None,
        };
        build_unsigned_transaction(&mut sender, vec![recipient], false)
            .expect("the session builds the transaction");
        drop(conn);
        lock_wallet("test".into()).await.unwrap();
    }

    /// A temp wallet DB (no globals) holding one spendable output that
    /// `build_unsigned_transaction` has reserved. Seeded the way upstream's own tests do.
    /// Returns the temp dir (keep it alive), the pool, the output id and the pending row id.
    fn reserve_one_output() -> (
        tempfile::TempDir,
        minotari_wallet::db::SqlitePool,
        i64,
        String,
    ) {
        let (dir, pool, output_id) = funded_account("pass");
        let conn = pool.get().unwrap();
        let account = minotari_wallet::db::get_account_by_name(&conn, "test")
            .unwrap()
            .unwrap();
        let address = account.get_address(Network::LocalNet, "pass").unwrap();
        let mut sender = TransactionSender::new(
            pool.clone(),
            "test".to_string(),
            Zeroizing::new("pass".to_string()),
            Network::LocalNet,
            3,
        )
        .unwrap();
        let recipient = Recipient {
            address,
            amount: MicroMinotari(1_000),
            payment_id: None,
        };
        let (pending_id, _) =
            build_unsigned_transaction(&mut sender, vec![recipient], false).unwrap();

        drop(conn);
        (dir, pool, output_id, pending_id)
    }

    /// A temp wallet DB (no globals) with account "test" under `passphrase`, holding one
    /// spendable output. Returns the temp dir (keep it alive), the pool and the output id.
    fn funded_account(
        passphrase: &str,
    ) -> (tempfile::TempDir, minotari_wallet::db::SqlitePool, i64) {
        use minotari_wallet::db::{
            create_account, get_account_by_name, init_db, insert_output, insert_scanned_tip_block,
            mark_output_confirmed,
        };
        use tari_common_types::seeds::cipher_seed::CipherSeed;
        use tari_common_types::types::{FixedHash, PrivateKey};
        use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
        use tari_transaction_components::transaction_components::{MemoField, WalletOutput};

        let dir = tempfile::tempdir().unwrap();
        let pool = init_db(dir.path().join("wallet.db")).unwrap();
        let conn = pool.get().unwrap();

        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(CipherSeed::random()).unwrap());
        create_account(&conn, "test", &wallet, passphrase).unwrap();
        let account = get_account_by_name(&conn, "test").unwrap().unwrap();
        insert_scanned_tip_block(&conn, account.id, 200, &[0u8; 32]).unwrap();

        // One spendable output, mined and confirmed at 100 under a tip of 200.
        let output = WalletOutput::new_from_parts(
            Default::default(),
            MicroMinotari(1_000_000),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            0,
            Default::default(),
            Default::default(),
            MicroMinotari(0),
            None,
            MemoField::new_empty(),
            FixedHash::from([1u8; 32]),
            Default::default(),
        );
        let output_id = insert_output(
            &conn,
            account.id,
            &PrivateKey::default(),
            vec![1u8; 32],
            &output,
            100,
            &FixedHash::zero(),
            1_700_000_000,
            None,
            None,
            Default::default(),
            false,
        )
        .unwrap();
        mark_output_confirmed(&conn, &FixedHash::from([1u8; 32]), 100, &[0u8; 32]).unwrap();

        drop(conn);
        (dir, pool, output_id)
    }

    /// The pre-broadcast gate passes only on a positive "unlocked" from the send's own
    /// pool: locked, an unknown wallet and an unreadable database all read as locked.
    #[tokio::test]
    async fn the_send_gate_fails_closed() {
        use crate::api::db::{lock_wallet, unlock_wallet, GLOBAL_STATE_TEST_LOCK};

        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let (dir, pool, _) = funded_account("pass");
        assert!(!unlocked_in(&pool, "test"), "locked");
        let path = dir.path().join("wallet.db").to_string_lossy().into_owned();
        unlock_wallet(path, "test".into(), "pass".into()).unwrap();
        assert!(unlocked_in(&pool, "test"), "unlocked");
        assert!(!unlocked_in(&pool, "missing"), "unknown wallet");

        // A pool whose database cannot be read.
        let broken = tempfile::tempdir().unwrap();
        let broken_pool = minotari_wallet::db::init_db(broken.path().join("wallet.db")).unwrap();
        broken_pool
            .get()
            .unwrap()
            .execute_batch("DROP TABLE accounts")
            .unwrap();
        assert!(
            !unlocked_in(&broken_pool, "test"),
            "a database error reads as locked"
        );
        lock_wallet("test".into()).await.unwrap();
    }

    #[test]
    fn same_account_refuses_a_renamed_or_replaced_wallet() {
        use minotari_wallet::db::{create_account, get_account_by_name};
        use tari_common_types::seeds::cipher_seed::CipherSeed;
        use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};

        let (_dir, pool, _) = funded_account("pass");
        let conn = pool.get().unwrap();
        let id = get_account_by_name(&conn, "test").unwrap().unwrap().id;
        same_account(&conn, "test", id).expect("unchanged");

        conn.execute(
            "UPDATE accounts SET friendly_name = 'gone' WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(
            same_account(&conn, "test", id).unwrap_err().to_string(),
            "No accounts found for this wallet"
        );
        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(CipherSeed::random()).unwrap());
        create_account(&conn, "test", &wallet, "pass").unwrap();
        assert_eq!(
            same_account(&conn, "test", id).unwrap_err().to_string(),
            "Wallet Error: The wallet was renamed or replaced; try again"
        );
    }

    /// A lock between loading the signing keys and building the transaction: the build
    /// re-reads the keys with an empty passphrase. For an empty-passphrase wallet that
    /// fallback succeeds, and for any other it fails to decrypt; either way the send
    /// reports `Wallet is locked` and leaves no UTXO reserved.
    #[tokio::test]
    async fn a_lock_before_the_build_is_locked_and_reserves_nothing() {
        use crate::api::db::{
            lock_wallet, set_database_path, unlock_wallet, GLOBAL_STATE_TEST_LOCK,
        };

        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        for passphrase in ["", "pass"] {
            let (dir, pool, output_id) = funded_account(passphrase);
            let path = dir.path().join("wallet.db").to_string_lossy().into_owned();
            set_database_path(path.clone()).unwrap();
            unlock_wallet(path, "test".into(), passphrase.into()).unwrap();
            stored_key_manager("test").expect("unlocked: the keys load");
            let mut sender = create_transaction_sender("test", Network::LocalNet, 3).unwrap();
            let address = minotari_wallet::db::get_account_by_name(&pool.get().unwrap(), "test")
                .unwrap()
                .unwrap()
                .get_address(Network::LocalNet, passphrase)
                .unwrap();

            assert!(lock_wallet("test".into()).await.unwrap());
            let recipient = Recipient {
                address,
                amount: MicroMinotari(1_000),
                payment_id: None,
            };
            let Err(err) = build_while_unlocked(&mut sender, "test", vec![recipient], false) else {
                panic!("a send after a lock must not build (passphrase {passphrase:?})");
            };
            assert_eq!(
                err.to_string(),
                "Wallet is locked",
                "passphrase {passphrase:?}"
            );
            assert_eq!(
                output_status(&pool.get().unwrap(), output_id),
                "UNSPENT",
                "no reservation left (passphrase {passphrase:?})"
            );
        }
    }

    fn output_status(conn: &r2d2_sqlite::rusqlite::Connection, output_id: i64) -> String {
        conn.query_row(
            "SELECT status FROM outputs WHERE id = ?1",
            [output_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Exception to the DB-free unit-test rule: a reservation released by the wrong id
    /// leaves the user's UTXOs locked with no error, so this pins the id seam against a
    /// real temp DB.
    #[test]
    fn reservation_is_released_by_pending_row_id_not_idempotency_key() {
        use minotari_wallet::db::get_output_lock_request_id;

        let (_dir, pool, output_id, pending_id) = reserve_one_output();
        let conn = pool.get().unwrap();

        assert_eq!(output_status(&conn, output_id), "LOCKED");
        assert_eq!(
            get_output_lock_request_id(&conn, output_id).unwrap(),
            Some(pending_id.clone()),
            "the returned id must be the pending row that holds the lock"
        );

        // The idempotency key is a different value and releases nothing.
        let idempotency_key: String = conn
            .query_row(
                "SELECT idempotency_key FROM pending_transactions WHERE id = ?1",
                [&pending_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(idempotency_key, pending_id);
        assert!(!expire_and_unlock_pending_transaction(&conn, &idempotency_key).unwrap());
        assert_eq!(output_status(&conn, output_id), "LOCKED");

        // The pending row id releases the output.
        assert!(expire_and_unlock_pending_transaction(&conn, &pending_id).unwrap());
        assert_eq!(output_status(&conn, output_id), "UNSPENT");
        assert_eq!(get_output_lock_request_id(&conn, output_id).unwrap(), None);
    }

    /// Exception to the DB-free unit-test rule (see above): the sweep run at database
    /// init and at send/scan start releases a stale reservation but not a live one.
    #[test]
    fn expired_reservations_are_released_on_database_init() {
        let (_dir, pool, output_id, pending_id) = reserve_one_output();
        let conn = pool.get().unwrap();

        // Not expired yet: the sweep leaves it alone.
        crate::api::db::release_expired_reservations(&conn);
        assert_eq!(output_status(&conn, output_id), "LOCKED");

        // A reservation stranded long ago, e.g. by an app kill before recording.
        conn.execute(
            "UPDATE pending_transactions SET expires_at = '2000-01-01 00:00:00' WHERE id = ?1",
            [&pending_id],
        )
        .unwrap();
        crate::api::db::release_expired_reservations(&conn);
        assert_eq!(output_status(&conn, output_id), "UNSPENT");
    }

    #[test]
    fn adapter_resolves_network_and_passes_through_valid_inputs() {
        let details = details_with(
            deterministic_recipient_base58(Network::MainNet),
            1_000,
            None,
        );
        let validated = validate_inputs(&details).expect("valid inputs must succeed");
        assert_eq!(validated.recipients.len(), 1);
        assert_eq!(validated.recipients[0].amount, MicroMinotari(1_000));
        // Proves the adapter resolved `Some(MainNet)` via `parse_network`.
        assert_eq!(validated.network, Network::MainNet);
        assert_eq!(validated.confirmations, DEFAULT_CONFIRMATION_WINDOW);
    }

    #[test]
    fn adapter_honours_explicit_confirmation_window() {
        let details = details_with(
            deterministic_recipient_base58(Network::MainNet),
            1_000,
            Some(12),
        );
        let validated = validate_inputs(&details).expect("valid inputs must succeed");
        assert_eq!(validated.confirmations, 12);
    }

    #[test]
    fn multi_send_validates_every_recipient() {
        let good = deterministic_recipient_base58(Network::MainNet);
        let mut details = details_with(good.clone(), 1_000, None);
        details.recipients.push(SendRecipient {
            address: good.clone(),
            amount: 2_000,
            payment_id: Some("second".to_string()),
        });
        let validated = validate_inputs(&details).expect("two good recipients");
        assert_eq!(validated.recipients.len(), 2);
        assert_eq!(validated.recipients[1].amount, MicroMinotari(2_000));
        assert_eq!(
            validated.recipients[1].payment_id.as_deref(),
            Some("second")
        );

        // A bad second recipient fails the whole send with the frozen prefix.
        details.recipients[1].address = "not-a-valid-base58-address".to_string();
        let Err(err) = validate_inputs(&details) else {
            panic!("a bad second recipient must be rejected");
        };
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn send_all_ignores_amount_but_needs_exactly_one_recipient() {
        let good = deterministic_recipient_base58(Network::MainNet);
        let mut details = details_with(good.clone(), 0, None);
        details.send_all = true;
        assert!(validate_inputs(&details).is_ok(), "send-all ignores amount");

        details.recipients.push(SendRecipient {
            address: good,
            amount: 0,
            payment_id: None,
        });
        let Err(err) = validate_inputs(&details) else {
            panic!("send-all to two recipients must be rejected");
        };
        assert_eq!(
            err.to_string(),
            "Wallet Error: A send-all pays exactly one recipient"
        );

        details.recipients.clear();
        details.send_all = false;
        let Err(err) = validate_inputs(&details) else {
            panic!("no recipients must be rejected");
        };
        assert_eq!(
            err.to_string(),
            "Wallet Error: At least one recipient is required"
        );
    }
}
