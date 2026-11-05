use crate::api::db::lock_wallet_everywhere;
use crate::api::db::{get_db_connection, get_db_path};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::scanner::{begin_maintenance, stop_wallet_scans};
use crate::domain::address::{construct_wallet_address_details, split_hidden_words};
use crate::domain::keys::key_manager_from_cipher_seed;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::{get_accounts, WalletDbError};
use minotari_wallet::utils::delete_wallet::delete_wallet_with_conn;
use minotari_wallet::utils::init_wallet::{
    init_with_seed_words, init_with_spend_and_view_keys, init_with_view_key,
};
use minotari_wallet::utils::rename_wallet::rename_wallet_with_conn;
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
use tari_utilities::hex::Hex;
use tari_utilities::hidden::Hidden;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// **SECRET.** An opaque, zeroize-on-drop handle to a wallet's seed words.
///
/// Exposed to Dart as an opaque handle (`#[frb(opaque)]`): the words are never
/// copied across the bridge implicitly. Call [`reveal_seed_words`] to read them
/// out explicitly. The backing buffer is wiped on drop ([`ZeroizeOnDrop`]).
#[frb(opaque)]
#[derive(Clone, Debug, Zeroize, ZeroizeOnDrop)]
pub struct SensitiveSeeds {
    pub words: Vec<String>,
}

/// The public details returned after creating, restoring, or importing a wallet.
///
/// `tari_address` is the wallet's base58 dual address. `wallet_birthday` is the
/// cipher-seed birthday (a block-height-derived epoch the scanner starts from).
/// The key hexes are the public spend key and the **private** view key. For
/// view-only imports `seed_words` is `None`; otherwise it carries the
/// **secret** [`SensitiveSeeds`] handle.
#[frb]
pub struct WalletCreationDetails {
    pub tari_address: String,
    pub wallet_birthday: u16,
    pub spend_public_key_hex: String,
    pub view_private_key_hex: String,
    pub seed_words: Option<SensitiveSeeds>,
}

/// Create a brand-new wallet from a freshly generated random cipher seed.
///
/// `wallet_name` names the account in the DB; `network` selects the Tari network
/// (`None` resolves to MainNet — frozen behavior); `passphrase` (**secret**)
/// encrypts the wallet at rest. Requires a database path (`set_database_path` or `unlock_wallet`).
/// Returns the new wallet's address, keys, and freshly generated seed words.
/// Synchronous.
#[frb]
pub fn create_wallet(
    wallet_name: String,
    network: Option<TariNetwork>,
    passphrase: String,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);

    let network = parse_network(network);
    apply_network(network)?;
    let seed = CipherSeed::random();
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

/// Restore an existing wallet from its BIP-39-style seed words.
///
/// `seed_words` (**secret**) is the mnemonic to restore from; `passphrase`
/// (**secret**) re-encrypts the restored wallet; `network` selects the network
/// (`None` → MainNet). Requires a database path (`set_database_path` or `unlock_wallet`). Returns the
/// restored wallet's address, keys, and seed words. Synchronous; errors on
/// invalid seed words.
#[frb]
pub fn restore_wallet(
    wallet_name: String,
    seed_words: Vec<String>,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let seed_words = Zeroizing::new(seed_words);

    let network = parse_network(network);
    apply_network(network)?;

    // These `.context(...)` strings are Dart-visible errors ("Invalid seed words" /
    // "Invalid cipher seed"); keep them.
    let mnemonic_string = Zeroizing::new(seed_words.join(" "));
    let mnemonic = SeedWords::from_str(&mnemonic_string).context("Invalid seed words")?;
    let seed = CipherSeed::from_mnemonic(&mnemonic, None).context("Invalid cipher seed")?;
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

/// Import a view-only wallet from a view private key and spend public key.
///
/// A view-only wallet can see incoming funds but cannot spend. `view_private_key_hex`
/// (**secret**) and `spend_public_key_hex` are hex-encoded keys; `birthday` is the
/// scan start epoch; `passphrase` (**secret**) encrypts the wallet; `network`
/// selects the network (`None` → MainNet). Requires a database path (`set_database_path` or `unlock_wallet`).
/// The returned `seed_words` is always `None`. Synchronous; errors on invalid hex.
#[frb]
pub fn import_view_only_wallet(
    wallet_name: String,
    view_private_key_hex: String,
    spend_public_key_hex: String,
    birthday: u16,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let network = parse_network(network);
    apply_network(network)?;

    let db_path = get_db_path()?;

    init_with_view_key(
        &view_private_key_hex,
        &spend_public_key_hex,
        &password,
        &db_path,
        birthday,
        Some(&wallet_name),
    )
    .context("Failed to init view-only wallet")?;

    let view_key = PrivateKey::from_hex(&view_private_key_hex)
        .map_err(|_| WalletError::internal("Invalid hex for view key"))?;
    let spend_key = CompressedPublicKey::from_hex(&spend_public_key_hex)
        .map_err(|_| WalletError::internal("Invalid hex for spend key"))?;

    build_creation_details(spend_key, view_key, birthday, network, None)
}

/// Import a spendable wallet from its view and spend private keys.
///
/// Unlike [`import_view_only_wallet`], the wallet can send (once unlocked) with
/// [`send_transaction`](crate::api::send_transaction::send_transaction). `view_private_key_hex`
/// and `spend_private_key_hex` (both **secret**) are hex-encoded keys; `birthday` is
/// the scan start in days since the birthday genesis (see
/// [`birthday_from_date`](crate::api::height::birthday_from_date)); `passphrase`
/// (**secret**) encrypts the wallet; `network` selects the network (`None` →
/// MainNet). Requires a database path (`set_database_path` or `unlock_wallet`). The returned `seed_words` is
/// always `None` (a key-only wallet has no mnemonic). Synchronous; errors on
/// invalid hex, a zero key, or equal view and spend keys.
#[frb]
pub fn import_wallet_from_keys(
    wallet_name: String,
    view_private_key_hex: String,
    spend_private_key_hex: String,
    birthday: u16,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let view_private_key_hex = Zeroizing::new(view_private_key_hex);
    let spend_private_key_hex = Zeroizing::new(spend_private_key_hex);
    let network = parse_network(network);
    apply_network(network)?;

    let db_path = get_db_path()?;

    let address = init_with_spend_and_view_keys(
        &view_private_key_hex,
        &spend_private_key_hex,
        &password,
        &db_path,
        birthday,
        network,
        None,
        Some(&wallet_name),
    )
    .map_err(|e| WalletError::wallet(format!("Failed to import wallet from keys: {e:#}")))?;

    Ok(WalletCreationDetails {
        tari_address: address.to_base58(),
        wallet_birthday: birthday,
        spend_public_key_hex: address.public_spend_key().to_hex(),
        view_private_key_hex: view_private_key_hex.to_string(),
        seed_words: None,
    })
}

/// Re-encrypt wallet `wallet_name` under `new_passphrase`.
///
/// `old_passphrase` and `new_passphrase` are **secret**. A wrong `old_passphrase`
/// errors as `Invalid Passphrase` and changes nothing (a running scan keeps running).
/// Otherwise the wallet's running scan is stopped (and waited for) first, and a scan
/// started meanwhile waits until the change is done; start it again afterwards. Other
/// wallets' scans keep running. An unlocked wallet stays unlocked. An unknown wallet errors as `No accounts found for this wallet`.
/// Requires a database path (`set_database_path` or `unlock_wallet`).
#[frb]
pub async fn change_passphrase(
    wallet_name: String,
    old_passphrase: String,
    new_passphrase: String,
) -> Result<()> {
    let old_passphrase = Zeroizing::new(old_passphrase);
    let new_passphrase = Zeroizing::new(new_passphrase);

    // Check the old passphrase before touching a running scan.
    {
        let conn = get_db_connection()?;
        let accounts = get_accounts(&conn, Some(&wallet_name))?;
        let account = accounts.first().ok_or(WalletError::NoAccounts)?;
        account
            .verify_passphrase(&old_passphrase)
            .map_err(|e| match e {
                WalletDbError::DecryptionFailed(_) => WalletError::InvalidPassphrase,
                other => WalletError::database(other.to_string()),
            })?;
    }

    // Stop the scan, and keep new scans out until the rewrite is done.
    let _maintenance = begin_maintenance(&wallet_name).await?;
    stop_wallet_scans(&wallet_name).await?;
    let mut conn = get_db_connection()?;

    minotari_wallet::db::change_passphrase(
        &mut conn,
        &wallet_name,
        &old_passphrase,
        &new_passphrase,
    )
    .map_err(|e| match e {
        WalletDbError::DecryptionFailed(_) => WalletError::InvalidPassphrase,
        other => WalletError::database(other.to_string()),
    })?;

    Ok(())
}

/// Recover the seed words of wallet `wallet_name`, for the seed page.
///
/// `password` (**secret**) is the wallet's encryption passphrase, checked even while
/// the wallet is unlocked. This and [`reveal_seed_words`] are the only way to read the
/// mnemonic. Returns a **secret**, zeroize-on-drop
/// [`SensitiveSeeds`] handle; call [`reveal_seed_words`] to read the words.
/// Requires a database path (`set_database_path` or `unlock_wallet`). Synchronous; errors if there is no
/// account or no seed words (e.g. a view-only wallet).
#[frb]
pub fn get_seed_words(wallet_name: String, password: String) -> Result<SensitiveSeeds> {
    let password = Zeroizing::new(password);

    let conn = get_db_connection()?;
    let accounts = get_accounts(&conn, Some(&wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;

    let seed_words_obj = account
        .get_seed_words(&password)?
        .ok_or_else(|| WalletError::internal("Account does not have seed words"))?;

    let hidden_joined = Hidden::from(seed_words_obj.join(" "));
    let raw_words = split_hidden_words(hidden_joined);

    Ok(SensitiveSeeds { words: raw_words })
}

/// Explicitly read the seed words out of an opaque [`SensitiveSeeds`] handle.
///
/// This is the only way to move the **secret** words across the bridge as plain
/// strings — callers opt in deliberately. Synchronous; infallible.
#[frb]
pub fn reveal_seed_words(handle: &SensitiveSeeds) -> Vec<String> {
    handle.words.clone()
}

fn generate_details_from_seed(
    seed: &CipherSeed,
    network: Network,
) -> Result<WalletCreationDetails> {
    let wallet_birthday = seed.birthday();

    // Single key-manager factory (domain layer) — no duplicated derivation.
    let key_manager = key_manager_from_cipher_seed(seed.clone())?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();

    let mnemonic = seed.to_mnemonic(MnemonicLanguage::English, None)?;
    let raw_words = split_hidden_words(mnemonic.join(" "));
    let sensitive_seeds = SensitiveSeeds { words: raw_words };

    build_creation_details(
        spend_key.pub_key,
        view_key,
        wallet_birthday,
        network,
        Some(sensitive_seeds),
    )
}

/// Assemble the public `WalletCreationDetails` DTO from the domain address
/// computation. The DTO (the wire contract) stays in `api`; the domain only
/// returns the computed address/key values.
fn build_creation_details(
    public_spend_key: CompressedPublicKey,
    private_view_key: PrivateKey,
    birthday: u16,
    network: Network,
    seed_words: Option<SensitiveSeeds>,
) -> Result<WalletCreationDetails> {
    let address = construct_wallet_address_details(public_spend_key, private_view_key, network)?;

    Ok(WalletCreationDetails {
        tari_address: address.tari_address,
        wallet_birthday: birthday,
        spend_public_key_hex: address.spend_public_key_hex,
        view_private_key_hex: address.view_private_key_hex,
        seed_words,
    })
}

/// Delete the wallet account named `wallet_name` from the database.
///
/// Works with or without a session: the wallet is locked for every holder and its
/// scans are stopped first; with no pool open, a temporary one is used. Errors if the
/// delete fails.
#[frb]
pub async fn delete_wallet(wallet_name: String) -> Result<()> {
    lock_wallet_everywhere(&wallet_name).await?;
    let mut conn = get_db_connection()?;
    delete_wallet_with_conn(&mut conn, &wallet_name).context("Failed to delete wallet")?;
    Ok(())
}

/// List the friendly names of every wallet account in the database.
///
/// Requires a database path (`set_database_path` or `unlock_wallet`). Synchronous.
#[frb]
pub fn list_wallets() -> Result<Vec<String>> {
    let mut conn = get_db_connection()?;
    let accounts = get_accounts(&mut conn, None)
        .context("Failed to retrieve wallet accounts from database")?;

    let names = accounts.into_iter().map(|a| a.friendly_name).collect();

    Ok(names)
}

/// Rename a wallet account from `current_wallet_name` to `new_wallet_name`.
///
/// Works with or without a session: the wallet is locked for every holder and its
/// scans are stopped first; unlock it again under the new name. With no pool open, a
/// temporary one is used. Renaming a wallet to its own name errors with
/// `The new wallet name is the same as the current one` and changes nothing.
#[frb]
pub async fn rename_wallet(current_wallet_name: String, new_wallet_name: String) -> Result<()> {
    if current_wallet_name == new_wallet_name {
        return Err(WalletError::SameName.into());
    }
    lock_wallet_everywhere(&current_wallet_name).await?;
    let mut conn = get_db_connection()?;
    rename_wallet_with_conn(&mut conn, &current_wallet_name, &new_wallet_name)
        .context("Failed to rename wallet")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Rename/delete against temp databases. They touch the process-global database
    //! path and key sessions, so they hold `GLOBAL_STATE_TEST_LOCK`.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::api::db::{set_database_path, unlock_wallet, GLOBAL_STATE_TEST_LOCK};
    use minotari_wallet::db::{create_account, get_account_by_name};
    use minotari_wallet::session::is_unlocked;
    use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};

    fn new_database(dir: &tempfile::TempDir, wallets: &[&str]) -> String {
        let path = dir.path().join("wallet.db");
        let pool = minotari_wallet::init_db(path.clone()).unwrap();
        let conn = pool.get().unwrap();
        for name in wallets {
            let wallet = WalletType::SeedWords(
                SeedWordsWallet::construct_new(CipherSeed::random()).unwrap(),
            );
            create_account(&conn, name, &wallet, "pass").unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn rename_to_the_same_name_changes_nothing() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["rename-same"]);
        unlock_wallet(path, "rename-same".into(), "pass".into()).unwrap();
        let before = get_account_by_name(&get_db_connection().unwrap(), "rename-same")
            .unwrap()
            .unwrap();

        let err = rename_wallet("rename-same".into(), "rename-same".into())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "The new wallet name is the same as the current one"
        );

        let conn = get_db_connection().unwrap();
        let after = get_account_by_name(&conn, "rename-same").unwrap().unwrap();
        assert_eq!(
            after.encrypted_wallet, before.encrypted_wallet,
            "keys untouched"
        );
        assert!(
            is_unlocked(&conn, "rename-same").unwrap(),
            "session untouched"
        );
        drop(conn);
        crate::api::db::lock_wallet("rename-same".into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rename_and_delete_work_with_or_without_a_session() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = new_database(&dir, &["rd-unlocked", "rd-locked"]);

        // With a session: the wallet is locked first, and the pool closes with it.
        unlock_wallet(path.clone(), "rd-unlocked".into(), "pass".into()).unwrap();
        rename_wallet("rd-unlocked".into(), "rd-renamed".into())
            .await
            .unwrap();
        let conn = get_db_connection().unwrap();
        assert!(!is_unlocked(&conn, "rd-renamed").unwrap());
        drop(conn);

        // Without a session: a temporary pool on the recorded path.
        set_database_path(path).unwrap();
        delete_wallet("rd-locked".into()).await.unwrap();
        let mut wallets = list_wallets().unwrap();
        wallets.sort();
        assert_eq!(wallets, vec!["rd-renamed".to_string()]);
    }
}
