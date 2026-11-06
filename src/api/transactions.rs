use crate::api::{db::get_db_connection, error::WalletError, utils::format_micro_tari};
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::{
    db::get_displayed_transactions_paginated, get_accounts, utils::timestamp::format_timestamp,
};
/// Fee charged on a transaction: `amount` in **microTari** plus a pre-formatted
/// `amount_display` string for the UI.
#[frb]
#[derive(Debug, Clone)]
pub struct FeeInfoDto {
    pub amount: u64,
    pub amount_display: String,
}

impl From<minotari_wallet::transactions::FeeInfo> for FeeInfoDto {
    fn from(f: minotari_wallet::transactions::FeeInfo) -> Self {
        Self {
            amount: f.amount.0,
            amount_display: format_micro_tari(f.amount.0),
        }
    }
}

/// Where a transaction sits on chain: `block_height`, a formatted `timestamp`
/// string, and the number of `confirmations` (blocks) on top of it.
#[frb]
#[derive(Debug, Clone)]
pub struct BlockchainInfoDto {
    pub block_height: u64,
    pub timestamp: String,
    pub confirmations: u64,
}

impl From<minotari_wallet::transactions::BlockchainInfo> for BlockchainInfoDto {
    fn from(i: minotari_wallet::transactions::BlockchainInfo) -> Self {
        Self {
            block_height: i.block_height,
            timestamp: format_timestamp(i.timestamp),
            confirmations: i.confirmations,
        }
    }
}

/// The other party to a transaction: their base58 `address` and the same
/// address rendered as an `address_emoji` string.
#[frb]
#[derive(Debug, Clone)]
pub struct CounterpartyInfoDto {
    pub address: String,
    pub address_emoji: String,
}

impl From<tari_common_types::tari_address::TariAddress> for CounterpartyInfoDto {
    fn from(addr: tari_common_types::tari_address::TariAddress) -> Self {
        Self {
            address: addr.to_base58(),
            address_emoji: addr.to_emoji_string(),
        }
    }
}

/// Whether a transaction credited (`Incoming`) or debited (`Outgoing`) the wallet.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionDirection {
    Incoming,
    Outgoing,
}

impl From<minotari_wallet::transactions::TransactionDirection> for DisplayedTransactionDirection {
    fn from(d: minotari_wallet::transactions::TransactionDirection) -> Self {
        match d {
            minotari_wallet::transactions::TransactionDirection::Incoming => Self::Incoming,
            minotari_wallet::transactions::TransactionDirection::Outgoing => Self::Outgoing,
        }
    }
}

/// How a transaction originated: a regular `Transfer`, mining `Coinbase`,
/// `OneSided` payment, or `Unknown`.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionSource {
    Transfer,
    Coinbase,
    OneSided,
    Unknown,
}

impl From<minotari_wallet::transactions::TransactionSource> for DisplayedTransactionSource {
    fn from(s: minotari_wallet::transactions::TransactionSource) -> Self {
        match s {
            minotari_wallet::transactions::TransactionSource::Transfer => Self::Transfer,
            minotari_wallet::transactions::TransactionSource::Coinbase => Self::Coinbase,
            minotari_wallet::transactions::TransactionSource::OneSided => Self::OneSided,
            minotari_wallet::transactions::TransactionSource::Unknown => Self::Unknown,
        }
    }
}

/// The display status of a transaction as shown in the wallet UI.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionStatus {
    Pending,
    Unconfirmed,
    Confirmed,
    Cancelled,
    Reorganized,
    Rejected,
    Locked,
}

impl From<minotari_wallet::transactions::TransactionDisplayStatus> for DisplayedTransactionStatus {
    fn from(s: minotari_wallet::transactions::TransactionDisplayStatus) -> Self {
        match s {
            minotari_wallet::transactions::TransactionDisplayStatus::Pending => Self::Pending,
            minotari_wallet::transactions::TransactionDisplayStatus::Unconfirmed => {
                Self::Unconfirmed
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Confirmed => Self::Confirmed,
            minotari_wallet::transactions::TransactionDisplayStatus::Cancelled => Self::Cancelled,
            minotari_wallet::transactions::TransactionDisplayStatus::Reorganized => {
                Self::Reorganized
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Rejected => Self::Rejected,
            minotari_wallet::transactions::TransactionDisplayStatus::Locked => Self::Locked,
        }
    }
}

/// A transaction as displayed in the wallet UI.
///
/// `amount` is in **microTari** with a pre-formatted `amount_display`. `counterparty`
/// and `fee` are absent for some kinds. `payrefs` are hex payment-reference hashes.
#[frb]
#[derive(Debug, Clone)]
pub struct DisplayedTransactionDto {
    pub id: String,
    pub direction: DisplayedTransactionDirection,
    pub source: DisplayedTransactionSource,
    pub status: DisplayedTransactionStatus,
    pub amount: u64,
    pub amount_display: String,
    /// Payer-controlled memo text, passed through unaltered (it can double as a
    /// payment ID). Render it as plain, bidi-isolated text.
    pub message: Option<String>,
    pub counterparty: Option<CounterpartyInfoDto>,
    pub blockchain: BlockchainInfoDto,
    pub fee: Option<FeeInfoDto>,
    pub payrefs: Vec<String>,
}

impl From<minotari_wallet::DisplayedTransaction> for DisplayedTransactionDto {
    fn from(t: minotari_wallet::DisplayedTransaction) -> Self {
        Self {
            id: t.id.to_string(),
            direction: t.direction.into(),
            source: t.source.into(),
            status: t.status.into(),
            amount: t.amount.0,
            amount_display: format_micro_tari(t.amount.0),
            message: t.message,
            counterparty: t.counterparty.map(CounterpartyInfoDto::from),
            blockchain: t.blockchain.into(),
            fee: t.fee.map(FeeInfoDto::from),
            payrefs: t
                .details
                .sent_payrefs
                .iter()
                .map(|fixed_hash| fixed_hash.to_string())
                .collect(),
        }
    }
}

/// List a page of the wallet's transactions, most recent first.
///
/// `limit`/`offset` paginate the result. Requires
/// a database path (`set_database_path` or `unlock_wallet`) first.
/// Synchronous; errors if the account does not exist.
#[frb]
pub fn get_transactions(
    wallet_name: String,
    limit: i64,
    offset: i64,
) -> Result<Vec<DisplayedTransactionDto>> {
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;

    let transactions = get_displayed_transactions_paginated(&conn, account.id, limit, offset)?;

    Ok(transactions.into_iter().map(Into::into).collect())
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests. Pure conversions, no I/O. These are upstream-drift
    //! tripwires: if `minotari`'s transaction enums add/rename a variant, the
    //! `From` impls (and these exhaustive assertions) must be revisited.
    //!
    //! The DTO enums derive only `Debug`/`Clone` (no `PartialEq`), so equality is
    //! asserted with `matches!` against the expected variant.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use minotari_wallet::transactions::{
        TransactionDirection, TransactionDisplayStatus, TransactionSource,
    };

    #[test]
    fn direction_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionDirection::from(TransactionDirection::Incoming),
            DisplayedTransactionDirection::Incoming
        ));
        assert!(matches!(
            DisplayedTransactionDirection::from(TransactionDirection::Outgoing),
            DisplayedTransactionDirection::Outgoing
        ));
    }

    #[test]
    fn source_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Transfer),
            DisplayedTransactionSource::Transfer
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Coinbase),
            DisplayedTransactionSource::Coinbase
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::OneSided),
            DisplayedTransactionSource::OneSided
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Unknown),
            DisplayedTransactionSource::Unknown
        ));
    }

    #[test]
    fn status_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Pending),
            DisplayedTransactionStatus::Pending
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Unconfirmed),
            DisplayedTransactionStatus::Unconfirmed
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Confirmed),
            DisplayedTransactionStatus::Confirmed
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Cancelled),
            DisplayedTransactionStatus::Cancelled
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Reorganized),
            DisplayedTransactionStatus::Reorganized
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Rejected),
            DisplayedTransactionStatus::Rejected
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Locked),
            DisplayedTransactionStatus::Locked
        ));
    }
}
