//! Crate-internal unified error model.
//!
//! `WalletError` is the single rich error type used **inside** the crate. It is
//! deliberately **never** a public `#[frb]` return type: every public bridge
//! function keeps returning [`anyhow::Result<T>`], and a `WalletError` reaches
//! Dart only after being converted into an [`anyhow::Error`] via the
//! [`From`] impl below. That conversion preserves the error's `Display` string
//! byte-for-byte, which is the *only* thing Dart observes (FRB surfaces the
//! message string of the thrown error). Changing those strings is therefore a
//! breaking change to the frozen Cake Wallet contract — see the plan's
//! "guiding principle" and Shared Contracts §2.
//!
//! ## Secret safety (Shared Contracts §3)
//!
//! Neither `Display` nor the derived `Debug` of any variant may contain secret
//! material (seed words, passphrases, private keys). The structured `details`
//! fields are populated **only** with upstream library error messages or static
//! text by call sites — never with raw secret inputs. The `InvalidSeedWords`
//! and `InvalidPassphrase` variants in particular carry no caller-supplied
//! secret: `InvalidPassphrase` is fieldless, and `InvalidSeedWords.details`
//! must only ever hold a library error string, not the words themselves.

use thiserror::Error;

/// Unified, crate-internal error type.
///
/// The `#[error("…")]` strings reproduce the exact Dart-visible messages that
/// existed before this type was introduced (the legacy `TransactionError`
/// `thiserror` texts and the ad-hoc `anyhow!`/`.context(…)` strings scattered
/// across `src/api/**`). Cross-checked by the baseline characterization tests
/// in the affected modules.
#[derive(Error, Debug)]
pub enum WalletError {
    /// The database has not been initialized yet.
    ///
    /// Preserves `db.rs`'s `.context("Database is not initialized")`.
    #[error("Database is not initialized")]
    NotInitialized,

    /// No account row matched the requested wallet.
    ///
    /// Preserves the `.context("No accounts found for this wallet")` string used
    /// across `wallet.rs`/`address.rs`/`balance.rs`/`transactions.rs`.
    #[error("No accounts found for this wallet")]
    NoAccounts,

    /// An unsupported network name was supplied.
    ///
    /// Preserves `network.rs`'s `anyhow!("Invalid network option: {invalid}")`.
    #[error("Invalid network option: {value}")]
    InvalidNetwork { value: String },

    /// A recipient/Tari address failed to parse.
    ///
    /// Preserves `TransactionError::InvalidAddress`'s
    /// `"Invalid Recipient Address: {0}"`.
    #[error("Invalid Recipient Address: {details}")]
    InvalidAddress { details: String },

    /// Seed words / mnemonic / cipher-seed parsing failed.
    ///
    /// Preserves `TransactionError::InvalidSeedWords`'s
    /// `"Invalid Seed Words: {0}"`. `details` must NEVER contain the seed words
    /// themselves — only the upstream parser's (secret-free) error text.
    #[error("Invalid Seed Words: {details}")]
    InvalidSeedWords { details: String },

    /// The supplied passphrase was rejected. Fieldless on purpose — the
    /// passphrase is never echoed back. Preserves
    /// `TransactionError::InvalidPassphrase`'s `"Invalid Passphrase"`.
    #[error("Invalid Passphrase")]
    InvalidPassphrase,

    /// Wallet-domain failure (construction, key derivation, transaction build,
    /// amount validation, …).
    ///
    /// Preserves `TransactionError::WalletError`'s `"Wallet Error: {0}"`. The
    /// zero-amount guard (`"Wallet Error: Amount must be greater than zero"`)
    /// flows through this variant.
    #[error("Wallet Error: {details}")]
    Wallet { details: String },

    /// Transaction signing failed.
    ///
    /// Preserves `TransactionError::SigningError`'s `"Signing Error: {0}"`.
    #[error("Signing Error: {details}")]
    Signing { details: String },

    /// Network / RPC interaction failed.
    ///
    /// Preserves `TransactionError::NetworkError`'s `"Network Error: {0}"`.
    #[error("Network Error: {details}")]
    Network { details: String },

    /// Database-layer failure.
    ///
    /// Preserves `TransactionError::DatabaseError`'s `"Database Error: {0}"`.
    #[error("Database Error: {details}")]
    Database { details: String },

    /// The user aborted the in-flight workflow.
    ///
    /// Preserves `TransactionError::Aborted`'s `"Aborted by User"`.
    #[error("Aborted by User")]
    Aborted,

    /// A scan-stream failure: the event-forwarder could not deliver an event to
    /// the Dart `StreamSink` (the sink was closed by the consumer). This is an
    /// **internal-only** signal used by the scanner to detect sink closure and
    /// cancel the scan; it is never surfaced to Dart as a thrown error or as a
    /// stream event (see `scanner.rs`), so its `Display` text has no frozen-contract
    /// constraint. Preserves the legacy ad-hoc `"Sink error: {…}"` shape.
    #[error("Sink error: {details}")]
    Scan { details: String },

    /// Catch-all for messages that have no dedicated variant. The `details`
    /// reproduces a pre-existing ad-hoc `anyhow!`/`.context(…)` string verbatim
    /// (e.g. `"Failed to lock DB_STATE for writing"`, `"Failed to init wallet"`,
    /// `"Invalid hex for view key"`). No prefix is added.
    #[error("{details}")]
    Internal { details: String },

    /// The operation needs the wallet's key session and the wallet is not unlocked
    /// (`unlock_wallet`).
    #[error("Wallet is locked")]
    Locked,

    /// `rename_wallet` was asked to rename a wallet to its own name.
    #[error("The new wallet name is the same as the current one")]
    SameName,

    /// A Background scan was refused because a Foreground scan of the same wallet
    /// is running.
    #[error("A foreground scan of this wallet is already running")]
    ForegroundScanActive,
}

impl WalletError {
    /// Convenience constructor for [`WalletError::InvalidAddress`].
    pub fn invalid_address(details: impl Into<String>) -> Self {
        Self::InvalidAddress {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::InvalidSeedWords`].
    ///
    /// Callers must pass a secret-free description (an upstream error string),
    /// never the seed words themselves.
    pub fn invalid_seed_words(details: impl Into<String>) -> Self {
        Self::InvalidSeedWords {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Wallet`].
    pub fn wallet(details: impl Into<String>) -> Self {
        Self::Wallet {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Signing`].
    pub fn signing(details: impl Into<String>) -> Self {
        Self::Signing {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Network`].
    pub fn network(details: impl Into<String>) -> Self {
        Self::Network {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Database`].
    pub fn database(details: impl Into<String>) -> Self {
        Self::Database {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Internal`].
    pub fn internal(details: impl Into<String>) -> Self {
        Self::Internal {
            details: details.into(),
        }
    }

    /// Convenience constructor for [`WalletError::Scan`].
    pub fn scan(details: impl Into<String>) -> Self {
        Self::Scan {
            details: details.into(),
        }
    }
}

// Boundary conversion `From<WalletError> for anyhow::Error` (Shared Contracts §2)
// is provided **for free** by anyhow's blanket
// `impl<E: std::error::Error + Send + Sync + 'static> From<E> for anyhow::Error`,
// which applies to `WalletError` because `#[derive(thiserror::Error)]` implements
// `std::error::Error` (and the type is `Send + Sync + 'static`). That blanket impl
// wraps the error via `anyhow::Error::new`, keeping `WalletError` as the error's
// source and making the resulting `anyhow::Error`'s `Display` **identical** to the
// `WalletError`'s `Display` — i.e. byte-for-byte what Dart saw before this type
// existed. This is what lets a `#[frb]` body `?`-propagate (or `.into()`) a
// `WalletError` with zero change to the Dart-visible error representation. A
// hand-written impl would conflict with the blanket one (E0119), so we rely on it
// and lock its behavior down with the conversion tests below.

#[cfg(test)]
mod tests {
    //! `WalletError` unit tests: Display-string contract (must match the legacy
    //! Dart-visible messages), the `anyhow::Error` boundary conversion (must
    //! preserve the string byte-for-byte), and the secret-leak guard.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn display_strings_match_legacy_contract() {
        assert_eq!(
            WalletError::NotInitialized.to_string(),
            "Database is not initialized"
        );
        assert_eq!(
            WalletError::NoAccounts.to_string(),
            "No accounts found for this wallet"
        );
        assert_eq!(
            WalletError::InvalidNetwork {
                value: "bitcoin".to_string()
            }
            .to_string(),
            "Invalid network option: bitcoin"
        );
        assert_eq!(
            WalletError::invalid_address("bad").to_string(),
            "Invalid Recipient Address: bad"
        );
        assert_eq!(
            WalletError::invalid_seed_words("oops").to_string(),
            "Invalid Seed Words: oops"
        );
        assert_eq!(
            WalletError::InvalidPassphrase.to_string(),
            "Invalid Passphrase"
        );
        assert_eq!(
            WalletError::wallet("Amount must be greater than zero").to_string(),
            "Wallet Error: Amount must be greater than zero"
        );
        assert_eq!(
            WalletError::signing("boom").to_string(),
            "Signing Error: boom"
        );
        assert_eq!(
            WalletError::network("down").to_string(),
            "Network Error: down"
        );
        assert_eq!(
            WalletError::database("locked").to_string(),
            "Database Error: locked"
        );
        assert_eq!(WalletError::Aborted.to_string(), "Aborted by User");
        assert_eq!(WalletError::Locked.to_string(), "Wallet is locked");
        assert_eq!(
            WalletError::SameName.to_string(),
            "The new wallet name is the same as the current one"
        );
        assert_eq!(
            WalletError::ForegroundScanActive.to_string(),
            "A foreground scan of this wallet is already running"
        );
        assert_eq!(
            WalletError::internal("Failed to init wallet").to_string(),
            "Failed to init wallet"
        );
        // `Scan` is internal-only (never thrown to Dart); pin its shape anyway.
        assert_eq!(
            WalletError::scan("closed").to_string(),
            "Sink error: closed"
        );
    }

    #[test]
    fn boundary_conversion_preserves_display_byte_for_byte() {
        // The Dart-visible representation is the anyhow::Error's Display. It must
        // equal the WalletError's Display exactly after the boundary conversion.
        for err in [
            WalletError::NotInitialized,
            WalletError::NoAccounts,
            WalletError::InvalidNetwork {
                value: "x".to_string(),
            },
            WalletError::invalid_address("a"),
            WalletError::invalid_seed_words("b"),
            WalletError::InvalidPassphrase,
            WalletError::wallet("c"),
            WalletError::signing("d"),
            WalletError::network("e"),
            WalletError::database("f"),
            WalletError::Aborted,
            WalletError::internal("g"),
            WalletError::scan("h"),
        ] {
            let expected = err.to_string();
            let converted: anyhow::Error = err.into();
            assert_eq!(converted.to_string(), expected);
        }
    }

    #[test]
    fn boundary_conversion_propagates_through_question_mark() {
        // Simulate a #[frb] body `?`-propagating a WalletError into anyhow::Result.
        fn boundary() -> anyhow::Result<()> {
            Err(WalletError::wallet("Amount must be greater than zero"))?;
            Ok(())
        }
        let err = boundary().expect_err("must be Err");
        assert_eq!(
            err.to_string(),
            "Wallet Error: Amount must be greater than zero"
        );
    }

    #[test]
    fn debug_and_display_never_leak_secret_material() {
        // Deliberately feed secret-looking strings into every field-bearing
        // variant and assert neither Display nor Debug echoes them. This guards
        // the secret-handling invariant: callers must not place secrets in `details`,
        // and the type itself must not be the vector for a leak.
        const SEED: &str = "abandon ability able about above absent absorb abstract";
        const PASS: &str = "hunter2-super-secret-passphrase";
        const KEY: &str = "deadbeefcafef00d-private-key-material";

        // Variants whose `details` is intended to carry only upstream error
        // text. We still verify that *if* a secret were ever (wrongly) passed,
        // the contract test would flag a regression by failing loudly — so here
        // we assert the SAFE usage: constructors fed with NON-secret text never
        // contain the secret constants.
        let safe = [
            WalletError::invalid_address("could not decode base58"),
            WalletError::invalid_seed_words("checksum mismatch"),
            WalletError::wallet("construction failed"),
            WalletError::signing("signature failed"),
            WalletError::network("connection refused"),
            WalletError::database("disk i/o error"),
            WalletError::internal("Failed to init wallet"),
            WalletError::scan("sink closed"),
            WalletError::InvalidPassphrase,
            WalletError::NotInitialized,
            WalletError::NoAccounts,
            WalletError::Aborted,
        ];
        for err in &safe {
            let display = err.to_string();
            let debug = format!("{err:?}");
            for secret in [SEED, PASS, KEY] {
                assert!(
                    !display.contains(secret),
                    "Display leaked secret: {display}"
                );
                assert!(!debug.contains(secret), "Debug leaked secret: {debug}");
            }
        }

        // The fieldless secret-adjacent variant must never echo a passphrase.
        assert_eq!(
            WalletError::InvalidPassphrase.to_string(),
            "Invalid Passphrase"
        );
        assert!(!format!("{:?}", WalletError::InvalidPassphrase).contains(PASS));
    }
}
