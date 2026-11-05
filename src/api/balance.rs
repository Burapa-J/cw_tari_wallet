use crate::api::db::get_db_connection;
use crate::api::error::WalletError;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::{get_accounts, get_balance as get_wallet_balance};

/// A wallet account's balance, all amounts in **microTari** (µT, 1e-6 XTM).
///
/// `total` is everything owned; `unconfirmed` is not yet confirmed;
/// `locked` is committed to in-flight spends; `available` is spendable now.
#[frb]
#[derive(Clone, Debug)]
pub struct AccountBalanceDto {
    pub total: u64,
    pub unconfirmed: u64,
    pub locked: u64,
    pub available: u64,
}

impl From<minotari_wallet::db::AccountBalance> for AccountBalanceDto {
    fn from(b: minotari_wallet::db::AccountBalance) -> Self {
        Self {
            total: b.total.0,
            unconfirmed: b.unconfirmed.0,
            locked: b.locked.0,
            available: b.available.0,
        }
    }
}

/// Read the balance of the wallet account named `wallet_name`.
///
/// Requires a database path (`set_database_path` or `unlock_wallet`) first.
/// Synchronous; errors if the account does not exist. Amounts are in microTari.
#[frb]
pub fn get_balance(wallet_name: String) -> Result<AccountBalanceDto> {
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;
    let agg_result = get_wallet_balance(&mut conn, account.id)?;
    Ok(agg_result.into())
}
