//! Send-transaction input validation — pure, returns typed [`WalletError`].
//!
//! Takes already-resolved primitives (the `network` comes pre-resolved from
//! `api::network::parse_network`, the rest straight off the public send DTOs) and
//! returns the parsed recipient address the `api` layer feeds into the transaction
//! sender. No DB, no global state.

use crate::api::error::WalletError;
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;

/// Validate one payee of a send.
///
/// - parses `address` from base58,
/// - rejects an address whose encoded network does not match `network`,
/// - rejects a zero `amount`, unless `send_all` (the amount is then worked out by
///   the sender).
///
/// Returns typed [`WalletError`] variants whose `Display` strings are the frozen
/// Dart-visible messages (Shared Contracts §2).
pub(crate) fn validate_recipient(
    network: Network,
    address: &str,
    amount: u64,
    send_all: bool,
) -> Result<TariAddress, WalletError> {
    let address = TariAddress::from_base58(address)
        .map_err(|e| WalletError::invalid_address(e.to_string()))?;

    if address.network() != network {
        return Err(WalletError::invalid_address(format!(
            "address network ({}) does not match the configured network ({})",
            address.network(),
            network
        )));
    }

    if amount == 0 && !send_all {
        return Err(WalletError::wallet("Amount must be greater than zero"));
    }

    Ok(address)
}

/// Reject an empty recipient list, and a send-all with other than one recipient.
pub(crate) fn validate_recipient_count(count: usize, send_all: bool) -> Result<(), WalletError> {
    if count == 0 {
        return Err(WalletError::wallet("At least one recipient is required"));
    }
    if send_all && count != 1 {
        return Err(WalletError::wallet("A send-all pays exactly one recipient"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Pure validation tests, re-homed from `send_transaction.rs`. They
    //! now assert the domain returns `WalletError` variants directly (the boundary
    //! conversion to the Dart-visible string is covered in `error.rs`). The
    //! known-good recipient address is derived deterministically from fixed key
    //! bytes (NOT a real funded address), so the suite stays hermetic.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::types::CompressedPublicKey;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

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

    #[test]
    fn rejects_zero_amount() {
        let recipient = deterministic_recipient_base58(Network::MainNet);
        let err = validate_recipient(Network::MainNet, &recipient, 0, false)
            .expect_err("zero amount must be rejected");
        // BASELINE CONTRACT: after the boundary conversion this must Display as
        // "Wallet Error: Amount must be greater than zero" (asserted in error.rs).
        assert!(
            matches!(err, WalletError::Wallet { ref details } if details == "Amount must be greater than zero")
        );
    }

    #[test]
    fn send_all_accepts_zero_amount() {
        let recipient = deterministic_recipient_base58(Network::MainNet);
        validate_recipient(Network::MainNet, &recipient, 0, true)
            .expect("a send-all ignores the amount");
    }

    #[test]
    fn rejects_malformed_recipient_address() {
        let err = validate_recipient(Network::MainNet, "not-a-valid-base58-address", 1_000, false)
            .expect_err("bad address must be rejected");
        // BASELINE CONTRACT: maps to "Invalid Recipient Address: …" at the boundary.
        assert!(
            matches!(err, WalletError::InvalidAddress { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn accepts_matching_network_with_nonzero_amount() {
        for network in [Network::MainNet, Network::Esmeralda] {
            let recipient = deterministic_recipient_base58(network);
            let address = validate_recipient(network, &recipient, 1_000, false)
                .expect("valid inputs must succeed");
            assert_eq!(address.network(), network);
        }
    }

    #[test]
    fn rejects_recipient_on_wrong_network() {
        // Recipient encoded for Esmeralda, but the wallet is configured for MainNet.
        let recipient = deterministic_recipient_base58(Network::Esmeralda);
        let err = validate_recipient(Network::MainNet, &recipient, 1_000, false)
            .expect_err("a cross-network recipient must be rejected");
        // BASELINE CONTRACT: maps to "Invalid Recipient Address: …" at the boundary.
        assert!(
            matches!(err, WalletError::InvalidAddress { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn recipient_count_rules() {
        assert!(validate_recipient_count(1, false).is_ok());
        assert!(validate_recipient_count(3, false).is_ok());
        assert!(validate_recipient_count(1, true).is_ok());

        let empty = validate_recipient_count(0, false).expect_err("no recipients");
        assert_eq!(
            empty.to_string(),
            "Wallet Error: At least one recipient is required"
        );
        let many = validate_recipient_count(2, true).expect_err("send-all to two");
        assert_eq!(
            many.to_string(),
            "Wallet Error: A send-all pays exactly one recipient"
        );
    }
}
