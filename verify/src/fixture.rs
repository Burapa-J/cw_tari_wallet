//! Deterministic, **view-only** fixture wallet, plus its golden expected values.
//!
//! ## Provenance & safety (read this before touching the fixture)
//! The fixture is built **programmatically and deterministically** — it is NOT a
//! capture of a real, funded wallet. It holds a **view-only** account (a view
//! private key + spend public key derived from fixed, non-secret test bytes) and
//! NO seed words / spend keys: it is structurally impossible to spend from it, so
//! committing it leaks nothing. The balance and the single transaction are
//! synthetic rows inserted via the minotari DB layer so the read APIs have known
//! golden values to assert against.
//!
//! ## Regenerating the committed DB
//! The committed asset `verify/fixtures/wallet.db` is produced by:
//!
//! ```sh
//! cargo run -p verify -- gen-fixture
//! # (or: make record-fixtures)
//! ```
//!
//! which calls [`build_fixture_db`] against the committed path. The Tier A tests
//! do not require the committed file — [`materialize_fixture_db`] rebuilds an
//! identical DB into a temp dir at test time — but the committed copy documents
//! the asset and lets a human inspect it.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::DateTime;
use minotari_wallet::db::{
    get_account_by_name, init_db, insert_balance_change, insert_displayed_transaction,
    insert_output, insert_scanned_tip_block, mark_output_confirmed,
};
use minotari_wallet::models::BalanceChange;
use minotari_wallet::transactions::{
    BlockchainInfo, DisplayedTransaction, TransactionDetails, TransactionDirection,
    TransactionDisplayStatus, TransactionSource,
};
use minotari_wallet::utils::init_wallet::init_with_view_key;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
use tari_common_types::types::{CompressedPublicKey, FixedHash};
use tari_crypto::ristretto::RistrettoSecretKey;
use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
use tari_transaction_components::test_helpers::create_test_input;
use tari_transaction_components::MicroMinotari;
use tari_utilities::hex::Hex;
use tari_utilities::ByteArray;

// ---------------------------------------------------------------------------
// Golden constants — the single source of truth for the Tier A assertions.
// ---------------------------------------------------------------------------

/// The network the fixture wallet/address is encoded for. Esmeralda (testnet) is
/// used deliberately so the fixture is never confusable with a mainnet wallet.
pub const FIXTURE_NETWORK: Network = Network::Esmeralda;

/// The account name in the fixture DB.
pub const WALLET_NAME: &str = "verify-fixture";

/// The wallet birthday (cipher-seed epoch) the view-only import was created with.
pub const BIRTHDAY: u16 = 100;

/// Fixed view **private** key (hex). Derived from non-secret test byte `7`; this
/// is a throwaway test key, not a real wallet secret.
pub const VIEW_PRIVATE_KEY_HEX: &str =
    "0700000000000000000000000000000000000000000000000000000000000000";

/// The little-endian byte value of the throwaway spend secret scalar (byte `11`),
/// from which [`spend_public_key_hex`] derives the spend **public** key.
const SPEND_SECRET_FIRST_BYTE: u8 = 11;

/// Derive the fixed spend **public** key hex from the throwaway spend secret. Kept
/// as a function (not a `const`) because the curve math is not `const`; it is
/// deterministic, so the recorder and `import_view_only_wallet` always agree.
pub fn spend_public_key_hex() -> String {
    let mut spend_bytes = [0u8; 32];
    spend_bytes[0] = SPEND_SECRET_FIRST_BYTE;
    let spend_sk = RistrettoSecretKey::from_canonical_bytes(&spend_bytes)
        .expect("fixed spend-key bytes must be a canonical scalar");
    CompressedPublicKey::from_secret_key(&spend_sk).to_hex()
}

// Golden balance (microTari). Inserted as two credits and one debit so every
// field of `AccountBalanceDto` is exercised with a non-trivial value.
/// Total credited to the account.
pub const CREDIT_A: u64 = 5_000_000; // 5 XTM
/// A second credit.
pub const CREDIT_B: u64 = 2_500_000; // 2.5 XTM
/// Amount debited (an outgoing spend recorded in history).
pub const DEBIT: u64 = 1_000_000; // 1 XTM

/// Golden total balance = credits − debits.
pub const GOLDEN_TOTAL: u64 = CREDIT_A + CREDIT_B - DEBIT; // 6_500_000

/// The single golden transaction's id (as the string the DTO exposes).
pub const TX_ID: u64 = 42;
/// The golden transaction's net amount (microTari).
pub const TX_AMOUNT: u64 = CREDIT_A;
/// The golden transaction's block height.
pub const TX_BLOCK_HEIGHT: u64 = 12_345;
/// The golden transaction's confirmation count.
pub const TX_CONFIRMATIONS: u64 = 7;
/// A fixed timestamp (Unix seconds) for the golden transaction, so the formatted
/// timestamp string in the DTO is deterministic.
pub const TX_TIMESTAMP: i64 = 1_700_000_000; // 2023-11-14T22:13:20Z

/// Generate a valid BIP39 mnemonic for a fresh, random wallet.
///
/// Used to supply seed words that are well-formed but belong to some *other* wallet, so
/// a test can exercise the seed/account mismatch path. Generated per call so no real
/// seed phrase is ever committed to the repository.
pub fn random_seed_words() -> Vec<String> {
    let seed = CipherSeed::random();
    let mnemonic = seed
        .to_mnemonic(MnemonicLanguage::English, None)
        .expect("a random cipher seed must produce a valid mnemonic");
    mnemonic
        .join(" ")
        .reveal()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect()
}

/// Height of the scanned tip [`add_spendable_outputs`] records; its outputs are mined
/// well below it, so they count as confirmed.
pub const FUNDED_TIP_HEIGHT: i64 = 200;

/// The block hash at `height` on the tests' mock chain. [`add_spendable_outputs`]
/// records its scanned tip with it, so a scan against that chain resumes cleanly.
pub fn chain_block_hash(height: u64) -> Vec<u8> {
    let mut hash = vec![0x5A; 32];
    hash.splice(0..8, height.to_be_bytes());
    hash
}

/// Give the seed-words wallet `wallet_name` (in the DB at `db_path`) one spendable,
/// confirmed output per entry of `values` (microTari), plus a scanned tip.
///
/// The outputs are built with the wallet's own key manager, so it can sign for them.
/// They exist only in this DB: no chain has them, so a node would reject a send.
pub fn add_spendable_outputs(
    db_path: &Path,
    wallet_name: &str,
    passphrase: &str,
    values: &[u64],
) -> Result<()> {
    let pool = init_db(db_path.to_path_buf()).context("Failed to open DB pool")?;
    let conn = pool.get().context("Failed to get DB connection")?;
    let account = get_account_by_name(&conn, wallet_name)?.context("wallet not found")?;
    let key_manager = account.get_key_manager(passphrase)?;
    let view_key = key_manager.get_private_view_key();

    insert_scanned_tip_block(
        &conn,
        account.id,
        FUNDED_TIP_HEIGHT,
        &chain_block_hash(FUNDED_TIP_HEIGHT as u64),
    )?;
    for value in values {
        let output = create_test_input(MicroMinotari(*value), 0, &key_manager, Vec::new(), None);
        insert_output(
            &conn,
            account.id,
            &view_key,
            output.output_hash().to_vec(),
            &output,
            10,
            &FixedHash::default(),
            1_700_000_000,
            None,
            None,
            FixedHash::default(),
            false,
        )?;
        mark_output_confirmed(&conn, &output.output_hash(), 13, &[0u8; 32])?;
    }
    Ok(())
}

/// Build the deterministic view-only fixture DB at `db_path` from scratch
/// (overwriting any existing file). Used by both the recorder (`gen-fixture`) and
/// the Tier A tests (into a temp dir).
///
/// The DB is seeded by:
/// 1. `init_with_view_key` — the **real public import path's** underlying call —
///    to create the encrypted view-only account, then
/// 2. direct minotari DB inserts of synthetic balance-change + displayed-tx rows
///    so the read APIs have golden values.
pub fn build_fixture_db(db_path: &Path) -> Result<()> {
    // Start clean: remove any stale file (and its WAL/SHM siblings) so the build
    // is reproducible.
    remove_db_files(db_path);

    // Step 1: create the encrypted view-only account via the library's underlying
    // import call. An empty passphrase keeps the fixture reproducible (it is a
    // throwaway test wallet with no funds).
    init_with_view_key(
        VIEW_PRIVATE_KEY_HEX,
        &spend_public_key_hex(),
        "", // passphrase: empty for the throwaway fixture
        db_path,
        BIRTHDAY,
        Some(WALLET_NAME),
    )
    .context("Failed to create view-only fixture account")?;

    // Step 2: open the same DB through the minotari pool and seed history rows.
    let pool = init_db(db_path.to_path_buf()).context("Failed to open fixture DB pool")?;
    let conn = pool.get().context("Failed to get fixture DB connection")?;

    // Resolve the real account id rather than assuming `1`, so the synthetic rows
    // can never silently attach to the wrong (or a non-existent) account.
    let account_id = minotari_wallet::db::get_accounts(&conn, Some(WALLET_NAME))
        .context("Failed to look up fixture account")?
        .first()
        .map(|a| a.id)
        .context("fixture account was not created")?;

    insert_credit(&conn, account_id, CREDIT_A, 10)?;
    insert_credit(&conn, account_id, CREDIT_B, 20)?;
    insert_debit(&conn, account_id, DEBIT, 30)?;

    insert_golden_transaction(&conn, account_id)?;

    Ok(())
}

/// Build the fixture into a fresh temp directory and return the DB path together
/// with the owning [`tempfile::TempDir`] (kept alive by the caller). Tier A tests
/// use this so they never mutate the committed asset.
pub fn materialize_fixture_db() -> Result<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir().context("Failed to create temp dir for fixture")?;
    let db_path = dir.path().join("wallet.db");
    build_fixture_db(&db_path)?;
    Ok((dir, db_path))
}

fn remove_db_files(db_path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db_path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
}

fn naive(ts: i64) -> chrono::NaiveDateTime {
    DateTime::from_timestamp(ts, 0)
        .expect("fixed fixture timestamp must be valid")
        .naive_utc()
}

fn insert_credit(
    conn: &rusqlite::Connection,
    account_id: i64,
    amount: u64,
    height: u64,
) -> Result<()> {
    insert_balance_change(
        conn,
        &BalanceChange {
            account_id,
            caused_by_output_id: None,
            caused_by_input_id: None,
            description: "fixture credit".to_string(),
            balance_credit: MicroMinotari(amount),
            balance_debit: MicroMinotari(0),
            effective_date: naive(TX_TIMESTAMP),
            effective_height: height,
            claimed_recipient_address: None,
            claimed_sender_address: None,
            memo_parsed: None,
            memo_hex: None,
            claimed_fee: None,
            claimed_amount: Some(MicroMinotari(amount)),
            is_reversal: false,
            reversal_of_balance_change_id: None,
            is_reversed: false,
        },
    )
    .context("Failed to insert fixture credit")?;
    Ok(())
}

fn insert_debit(
    conn: &rusqlite::Connection,
    account_id: i64,
    amount: u64,
    height: u64,
) -> Result<()> {
    insert_balance_change(
        conn,
        &BalanceChange {
            account_id,
            caused_by_output_id: None,
            caused_by_input_id: None,
            description: "fixture debit".to_string(),
            balance_credit: MicroMinotari(0),
            balance_debit: MicroMinotari(amount),
            effective_date: naive(TX_TIMESTAMP),
            effective_height: height,
            claimed_recipient_address: None,
            claimed_sender_address: None,
            memo_parsed: None,
            memo_hex: None,
            claimed_fee: None,
            claimed_amount: Some(MicroMinotari(amount)),
            is_reversal: false,
            reversal_of_balance_change_id: None,
            is_reversed: false,
        },
    )
    .context("Failed to insert fixture debit")?;
    Ok(())
}

fn insert_golden_transaction(conn: &rusqlite::Connection, account_id: i64) -> Result<()> {
    let tx = DisplayedTransaction {
        id: TX_ID.into(),
        direction: TransactionDirection::Incoming,
        source: TransactionSource::Coinbase,
        status: TransactionDisplayStatus::Confirmed,
        amount: MicroMinotari(TX_AMOUNT),
        message: Some("fixture coinbase".to_string()),
        counterparty: None,
        blockchain: BlockchainInfo {
            block_height: TX_BLOCK_HEIGHT,
            timestamp: naive(TX_TIMESTAMP),
            confirmations: TX_CONFIRMATIONS,
            block_hash: FixedHash::zero(),
        },
        fee: None,
        details: TransactionDetails {
            account_id,
            total_credit: MicroMinotari(TX_AMOUNT),
            total_debit: MicroMinotari(0),
            inputs: Vec::new(),
            outputs: Vec::new(),
            output_type: None,
            coinbase_extra: None,
            memo_hex: None,
            sent_output_hashes: Vec::new(),
            sent_payrefs: Vec::new(),
        },
        lock_height: 0,
    };

    insert_displayed_transaction(conn, &tx).context("Failed to insert golden transaction")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tier B — recorded RPC fixtures.
//
// The base-node read APIs (`get_tip_info`, `is_node_synced`) do a plain
// `GET /get_tip_info` and deserialize a `TipInfoResponse`. We "record" the RPC by
// serializing a real `TipInfoResponse` to JSON (so the committed JSON is exactly
// what the library deserializes — recording the *shape* the upstream type emits),
// then Tier B "replays" it from a local wiremock server. `make record-fixtures`
// rewrites the committed JSON when the upstream RPC shape changes.
// ---------------------------------------------------------------------------
pub mod rpc {
    use super::*;
    use primitive_types::U512;
    use std::path::Path;
    use tari_common_types::chain_metadata::ChainMetadata;
    use tari_transaction_components::rpc::models::TipInfoResponse;

    /// The golden tip height the recorded `get_tip_info` reports.
    pub const TIP_HEIGHT: u64 = 250_000;
    /// The golden pruning horizon.
    pub const PRUNING_HORIZON: u64 = 0;
    /// The golden pruned height (0 ⇒ archival).
    pub const PRUNED_HEIGHT: u64 = 0;
    /// The golden tip timestamp (Unix seconds).
    pub const TIP_TIMESTAMP: u64 = 1_700_500_000;
    /// First byte of the deterministic best-block hash.
    pub const BEST_BLOCK_HASH_FIRST_BYTE: u8 = 0xAB;

    /// Build the deterministic recorded `get_tip_info` response.
    ///
    /// `is_synced` toggles the two recorded captures (synced vs. catching-up).
    pub fn tip_info_response(is_synced: bool) -> TipInfoResponse {
        let mut hash = [0u8; 32];
        hash[0] = BEST_BLOCK_HASH_FIRST_BYTE;
        let metadata = ChainMetadata::new(
            TIP_HEIGHT,
            FixedHash::new(hash),
            PRUNING_HORIZON,
            PRUNED_HEIGHT,
            U512::from(123_456_789u64),
            TIP_TIMESTAMP,
        )
        .expect("fixed chain-metadata values must be valid");
        TipInfoResponse {
            metadata: Some(metadata),
            is_synced,
        }
    }

    /// Filenames of the committed RPC captures (under `fixtures/rpc/`).
    pub const SYNCED_FILE: &str = "get_tip_info_synced.json";
    pub const UNSYNCED_FILE: &str = "get_tip_info_unsynced.json";

    /// Serialize the recorded responses to the committed `fixtures/rpc/` directory.
    /// Called by `make record-fixtures` / `verify record-fixtures`.
    pub fn record_to(dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        for (file, synced) in [(SYNCED_FILE, true), (UNSYNCED_FILE, false)] {
            let json = serde_json::to_string_pretty(&tip_info_response(synced))?;
            std::fs::write(dir.join(file), json + "\n")?;
        }
        Ok(())
    }

    /// The committed RPC JSON for the given file, read from `fixtures/rpc/`.
    /// Tier B serves this verbatim from wiremock.
    pub fn read_committed(file: &str) -> anyhow::Result<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("rpc")
            .join(file);
        Ok(std::fs::read_to_string(&path)?)
    }
}
