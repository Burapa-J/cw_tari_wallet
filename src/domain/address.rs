//! Address construction — pure, network-parameterised Tari address derivation.
//!
//! Given a public spend key and a private view key, this builds the dual Tari
//! address and the hex encodings the `api` layer needs. It returns a small
//! **internal** value struct ([`WalletAddressDetails`]); the public
//! `WalletCreationDetails` DTO is assembled by `api::wallet` from these values, so
//! the wire contract stays firmly in `api` (Shared Contracts §5).

use crate::api::error::WalletError;
use std::fmt;
use tari_common::configuration::Network;
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_crypto::compressed_key::CompressedKey;
use tari_utilities::hex::Hex;
use tari_utilities::hidden::Hidden;

/// Computed address/key values for a wallet. Internal to the crate — `api::wallet`
/// maps these into the public `WalletCreationDetails` DTO.
pub(crate) struct WalletAddressDetails {
    /// Base58-encoded dual Tari address.
    pub tari_address: String,
    /// Hex-encoded public spend key.
    pub spend_public_key_hex: String,
    /// Hex-encoded private view key.
    pub view_private_key_hex: String,
}

impl fmt::Debug for WalletAddressDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletAddressDetails")
            .field("tari_address", &self.tari_address)
            .field("spend_public_key_hex", &self.spend_public_key_hex)
            .field("view_private_key_hex", &"[REDACTED]")
            .finish()
    }
}

/// Build the dual Tari address (and hex key encodings) for the given keys and
/// network. Pure: takes `network` as a parameter, touches no global state.
pub(crate) fn construct_wallet_address_details(
    public_spend_key: CompressedPublicKey,
    private_view_key: PrivateKey,
    network: Network,
) -> Result<WalletAddressDetails, WalletError> {
    let public_view_key = CompressedKey::from_secret_key(&private_view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        public_spend_key.clone(),
        network,
        TariAddressFeatures::create_one_sided_only(),
        None,
    )
    // Error-message parity (gotcha): preserve the exact Dart-visible string
    // the original `.context("Failed to generate Tari address")` produced — no
    // prefix, no appended source — via the prefix-free `Internal` variant.
    .map_err(|_| WalletError::internal("Failed to generate Tari address"))?;

    Ok(WalletAddressDetails {
        tari_address: tari_address.to_base58(),
        spend_public_key_hex: public_spend_key.to_hex(),
        view_private_key_hex: private_view_key.to_hex(),
    })
}

/// Split a whitespace-joined hidden mnemonic back into its individual words.
///
/// Kept here (shared by wallet creation and seed-word retrieval) so the reveal of
/// the [`Hidden`] container happens in exactly one audited spot.
pub(crate) fn split_hidden_words(hidden: Hidden<String>) -> Vec<String> {
    hidden
        .reveal()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    //! Pure address-construction tests — fixed key bytes in, deterministic base58
    //! address out. No DB, no network state, no globals.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

    /// Build fixed (view, spend) keys from small canonical scalar bytes.
    fn fixed_keys() -> (CompressedPublicKey, PrivateKey) {
        let mut view_bytes = [0u8; 32];
        view_bytes[0] = 7;
        let mut spend_bytes = [0u8; 32];
        spend_bytes[0] = 11;

        let view_sk = RistrettoSecretKey::from_canonical_bytes(&view_bytes)
            .expect("fixed view-key bytes must be a canonical scalar");
        let spend_sk = RistrettoSecretKey::from_canonical_bytes(&spend_bytes)
            .expect("fixed spend-key bytes must be a canonical scalar");

        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);
        (spend_pk, view_sk)
    }

    #[test]
    fn known_keys_network_produce_golden_address() {
        let (spend_pk, view_sk) = fixed_keys();
        let details = construct_wallet_address_details(spend_pk, view_sk, Network::MainNet)
            .expect("valid keys must build an address");

        // Golden vector: the same fixed keys + MainNet must always round-trip to a
        // parseable dual address with these exact key hex encodings. The address is
        // re-parseable (proves it is well-formed base58).
        assert!(TariAddress::from_base58(&details.tari_address).is_ok());
        assert_eq!(
            details.view_private_key_hex,
            "0700000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn address_is_deterministic_for_fixed_inputs() {
        let (spend_pk, view_sk) = fixed_keys();
        let a = construct_wallet_address_details(spend_pk, view_sk, Network::MainNet).unwrap();
        let (spend_pk2, view_sk2) = fixed_keys();
        let b = construct_wallet_address_details(spend_pk2, view_sk2, Network::MainNet).unwrap();
        assert_eq!(a.tari_address, b.tari_address);
        assert_eq!(a.spend_public_key_hex, b.spend_public_key_hex);
    }

    #[test]
    fn network_changes_the_address() {
        let (spend_pk, view_sk) = fixed_keys();
        let main = construct_wallet_address_details(spend_pk, view_sk, Network::MainNet).unwrap();
        let (spend_pk2, view_sk2) = fixed_keys();
        let esme =
            construct_wallet_address_details(spend_pk2, view_sk2, Network::Esmeralda).unwrap();
        assert_ne!(main.tari_address, esme.tari_address);
    }

    #[test]
    fn split_hidden_words_splits_on_whitespace() {
        let hidden = Hidden::hide("abandon ability  able".to_string());
        assert_eq!(
            split_hidden_words(hidden),
            vec!["abandon", "ability", "able"]
        );
    }

    #[test]
    fn wallet_address_details_redacts_view_key_in_debug() {
        let (spend_pk, view_sk) = fixed_keys();
        let details =
            construct_wallet_address_details(spend_pk, view_sk, Network::MainNet).unwrap();
        let debug_repr = format!("{details:?}");
        assert!(debug_repr.contains("[REDACTED]"));
        assert!(!debug_repr.contains(&details.view_private_key_hex));
        assert!(debug_repr.contains(&details.tari_address));
        assert!(debug_repr.contains(&details.spend_public_key_hex));
    }
}
