use crate::api::config::{
    DEFAULT_BASE_URL, DEFAULT_CONFIRMATION_WINDOW, DEFAULT_NUM_OUTPUTS, SEND_FEE_PER_GRAM,
};
use crate::api::db::get_db_pool;
use crate::api::error::WalletError;
use crate::api::network::{parse_network, TariNetwork};
use crate::domain::validation::validate_recipient;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::fee_estimator::{FeeEstimator, FeePriority as LibFeePriority};
use tari_transaction_components::MicroMinotari;

/// Desired confirmation speed for a transaction, trading fee against latency:
/// `Slow` (cheapest), `Medium`, or `Fast` (most expensive).
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeePriority {
    Slow,
    Medium,
    Fast,
}

impl From<FeePriority> for LibFeePriority {
    fn from(p: FeePriority) -> Self {
        match p {
            FeePriority::Slow => LibFeePriority::Slow,
            FeePriority::Medium => LibFeePriority::Medium,
            FeePriority::Fast => LibFeePriority::Fast,
        }
    }
}

/// A fee estimate for a prospective send.
///
/// `estimated_fee` and `total_amount_required` (amount + fee) are in **microTari**;
/// `fee_per_gram` is the rate in microTari per gram of transaction weight;
/// `input_count` is how many UTXOs would be spent.
#[frb]
#[derive(Debug, Clone)]
pub struct FeeEstimate {
    pub estimated_fee: u64,
    pub total_amount_required: u64,
    pub fee_per_gram: u64,
    pub input_count: usize,
}

/// Estimate the fee to send `amount` (in **microTari**) at the given `priority`.
///
/// `base_url` is the base node RPC endpoint and `wallet_name` selects the funding
/// account. Requires
/// a database path (`set_database_path` or `unlock_wallet`) first. Async;
/// performs network I/O. Errors if estimation fails or no estimate matches the
/// requested priority.
#[frb]
pub async fn estimate_transaction_fee(
    amount: u64,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    estimate_fee(amount, DEFAULT_NUM_OUTPUTS, priority, base_url, wallet_name).await
}

/// Estimate the fee to pay `num_outputs` recipients a total of `amount` (in
/// **microTari**) in one transaction, at the given `priority`.
///
/// As [`estimate_transaction_fee`], which is this with `num_outputs = 1`. Use it to
/// quote a [`send_transaction`](crate::api::send_transaction::send_transaction).
#[frb]
pub async fn estimate_transaction_fee_for_outputs(
    amount: u64,
    num_outputs: u32,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    let num_outputs = usize::try_from(num_outputs)
        .map_err(|_| WalletError::wallet("Too many outputs to estimate a fee for"))?;
    estimate_fee(amount, num_outputs, priority, base_url, wallet_name).await
}

async fn estimate_fee(
    amount: u64,
    num_outputs: usize,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    let pool = get_db_pool()?;

    let estimator = FeeEstimator::new(pool, base_url);

    let amount_micro = MicroMinotari(amount);

    let estimates = estimator
        .estimate_fees(
            &wallet_name,
            amount_micro,
            num_outputs,
            DEFAULT_CONFIRMATION_WINDOW,
            None, // estimated_output_size: let the estimator use its own default
        )
        .await
        .context("Failed to estimate fees")?;

    let target_priority: LibFeePriority = priority.into();

    let selected_estimate = estimates
        .into_iter()
        .find(|e| e.priority == target_priority)
        .ok_or_else(|| {
            WalletError::internal("Could not find fee estimate for requested priority")
        })?;

    Ok(FeeEstimate {
        estimated_fee: selected_estimate.estimated_fee.0,
        total_amount_required: selected_estimate.total_amount_required.0,
        fee_per_gram: selected_estimate.fee_per_gram.0,
        input_count: selected_estimate.input_count,
    })
}

/// What a send-all would cost and pay, from [`estimate_send_all_fee`].
///
/// `fee` and `amount_after_fee` (what the recipient receives) are in **microTari**;
/// `input_count` is how many UTXOs it spends. `skipped_dust` outputs are left behind
/// for being worth less than they cost to spend, and `skipped_over_cap` for not
/// fitting one transaction (another send-all can sweep those).
#[frb]
#[derive(Debug, Clone)]
pub struct SendAllFeeEstimate {
    pub fee: u64,
    pub amount_after_fee: u64,
    pub input_count: usize,
    pub skipped_dust: usize,
    pub skipped_over_cap: usize,
}

/// Quote a send-all of wallet `wallet_name` to `recipient_address`.
///
/// Computed by minotari exactly as the send-all in
/// [`send_transaction`](crate::api::send_transaction::send_transaction)
/// computes it, at the same fee per gram, so `amount_after_fee` is what that send
/// pays. `payment_id` changes the output size and so the fee; pass the one the send
/// will use, and the same `confirmation_window` (`None` → 3, as for the send).
/// Offline: reads only the wallet database. `network` as elsewhere (`None` → MainNet). Errors if the address is invalid or the wallet's spendable
/// balance would leave the recipient only dust. Requires
/// a database path (`set_database_path` or `unlock_wallet`) first.
#[frb]
pub fn estimate_send_all_fee(
    wallet_name: String,
    recipient_address: String,
    payment_id: Option<String>,
    network: Option<TariNetwork>,
    confirmation_window: Option<u64>,
) -> Result<SendAllFeeEstimate> {
    let network = parse_network(network);
    let address = validate_recipient(network, &recipient_address, 0, true)?;

    // The estimator never contacts the node for a send-all quote.
    let estimator = FeeEstimator::new(get_db_pool()?, DEFAULT_BASE_URL.to_string());
    let estimate = estimator
        .estimate_fees_send_all(
            &wallet_name,
            network,
            &address,
            payment_id.as_deref(),
            MicroMinotari(SEND_FEE_PER_GRAM),
            confirmation_window.unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
        )
        .map_err(|e| WalletError::wallet(e.to_string()))?;

    Ok(SendAllFeeEstimate {
        fee: estimate.fee.0,
        amount_after_fee: estimate.amount_after_fee.0,
        input_count: estimate.input_count,
        skipped_dust: estimate.skipped_dust,
        skipped_over_cap: estimate.skipped_over_cap,
    })
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests. Pure conversion, no I/O. Upstream-drift tripwire:
    //! if `minotari`'s `FeePriority` adds/renames a variant, this must change.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fee_priority_maps_to_lib_priority_exhaustively() {
        assert_eq!(
            LibFeePriority::from(FeePriority::Slow),
            LibFeePriority::Slow
        );
        assert_eq!(
            LibFeePriority::from(FeePriority::Medium),
            LibFeePriority::Medium
        );
        assert_eq!(
            LibFeePriority::from(FeePriority::Fast),
            LibFeePriority::Fast
        );
    }
}
