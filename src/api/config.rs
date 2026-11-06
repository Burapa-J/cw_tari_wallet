//! Central configuration constants. Single source of truth — do not redefine elsewhere.
//!
//! Every numeric/string default consumed by the `api` layer lives here exactly once
//! (Shared Contracts §4). The values are deliberately byte-for-byte identical to the
//! previously scattered definitions in `fee.rs` and `send_transaction.rs`; this module
//! only de-duplicates them and is purely internal (`pub(crate)`), so it never becomes a
//! new public `#[frb]` surface.

/// Default number of confirmations required (blocks) when the caller does not specify
/// `confirmation_window`.
pub(crate) const DEFAULT_CONFIRMATION_WINDOW: u64 = 3;

/// Default base node RPC endpoint, applied only when the caller omits `base_url`.
///
/// Audited: this is the public Tari mainnet RPC endpoint. It is **not**
/// derived from the requested network — callers targeting a non-mainnet network already
/// pass an explicit `base_url`, and the only site that applies this default
/// (`send_transaction`) does so without consulting the resolved `Network`. Deriving the
/// URL from the network would change observable behavior and is a breaking,
/// Cake-Wallet-coordinated change deliberately deferred out of this step.
pub(crate) const DEFAULT_BASE_URL: &str = "https://rpc.tari.com";

/// Default wallet passphrase (empty string). Used as the fallback when the caller passes
/// `passphrase: None`; the empty-string semantics are part of the frozen behavior.
pub(crate) const DEFAULT_PASSPHRASE: &str = "";

/// UTXO lock duration when building a transaction, in seconds (24 hours).
pub(crate) const SECONDS_TO_LOCK_UTXO: u64 = 60 * 60 * 24;

/// Default number of outputs assumed for fee estimation.
pub(crate) const DEFAULT_NUM_OUTPUTS: usize = 1;

/// Response timeout (seconds) for the `check_node_health` probe. Short so a dead
/// node fails fast.
pub(crate) const HEALTH_TIMEOUT_SECS: u64 = 5;

/// Retry attempts for the `check_node_health` probe. Low for a snappy result.
pub(crate) const HEALTH_MAX_RETRIES: u32 = 1;

/// Tokio worker threads in the FRB bridge's async runtime.
pub(crate) const BRIDGE_ASYNC_WORKER_THREADS: usize = 2;

/// Name of the FRB bridge's tokio runtime threads.
pub(crate) const BRIDGE_ASYNC_THREAD_NAME: &str = "cw-tari-rt";

/// Threads in the FRB bridge's pool for sync-in-background calls.
pub(crate) const BRIDGE_THREAD_POOL_SIZE: usize = 2;

#[cfg(test)]
mod tests {
    //! Value-pinning guards (Shared Contracts §4). These assert the *resolved* default
    //! values are byte-for-byte what the scattered constants were before consolidation.
    //! Flipping any of these is an observable behavior change and must be treated as
    //! breaking.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn default_confirmation_window_is_three() {
        assert_eq!(DEFAULT_CONFIRMATION_WINDOW, 3);
    }

    #[test]
    fn default_base_url_is_tari_mainnet_rpc() {
        assert_eq!(DEFAULT_BASE_URL, "https://rpc.tari.com");
    }

    #[test]
    fn default_passphrase_is_empty() {
        assert_eq!(DEFAULT_PASSPHRASE, "");
    }

    #[test]
    fn seconds_to_lock_utxo_is_twenty_four_hours() {
        assert_eq!(SECONDS_TO_LOCK_UTXO, 60 * 60 * 24);
        assert_eq!(SECONDS_TO_LOCK_UTXO, 86_400);
    }

    #[test]
    fn default_num_outputs_is_one() {
        assert_eq!(DEFAULT_NUM_OUTPUTS, 1);
    }

    #[test]
    fn health_timeout_secs_is_five() {
        assert_eq!(HEALTH_TIMEOUT_SECS, 5);
    }

    #[test]
    fn health_max_retries_is_one() {
        assert_eq!(HEALTH_MAX_RETRIES, 1);
    }
}
