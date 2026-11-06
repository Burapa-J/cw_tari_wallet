//! Key-manager derivation — the single source of truth for turning a seed into a
//! [`KeyManager`].
//!
//! Wallet creation/restore (`api::wallet`) routes through this module, so the
//! `CipherSeed → SeedWordsWallet → WalletType → KeyManager` derivation exists in
//! exactly one place.

use crate::api::error::WalletError;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::KeyManager;

/// Build a [`KeyManager`] from an already-constructed [`CipherSeed`].
///
/// This is the lowest-level factory; callers that hold a `CipherSeed` directly
/// (e.g. a freshly generated random seed, or one decoded from a mnemonic) use it.
pub(crate) fn key_manager_from_cipher_seed(seed: CipherSeed) -> Result<KeyManager, WalletError> {
    let wallet_type = WalletType::SeedWords(
        SeedWordsWallet::construct_new(seed)
            .map_err(|e| WalletError::wallet(format!("Wallet construction failed: {}", e)))?,
    );

    KeyManager::new(wallet_type)
        .map_err(|e| WalletError::wallet(format!("Key Manager failed: {}", e)))
}

#[cfg(test)]
mod tests {
    //! Pure derivation tests — no DB, no network, no global state.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
    use tari_utilities::hex::Hex;

    #[test]
    fn same_cipher_seed_derives_identical_keys() {
        let seed = CipherSeed::random();
        let km1 = key_manager_from_cipher_seed(seed.clone()).expect("derives a key manager");
        let km2 = key_manager_from_cipher_seed(seed).expect("derives a key manager");
        assert_eq!(
            km1.get_private_view_key().to_hex(),
            km2.get_private_view_key().to_hex()
        );
        assert_eq!(
            km1.get_spend_key().pub_key.to_hex(),
            km2.get_spend_key().pub_key.to_hex()
        );
    }
}
