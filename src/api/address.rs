use crate::api::{
    db::{get_db_connection, unlocked_account},
    network::{apply_network, parse_network, TariNetwork},
};
use anyhow::Result;
use flutter_rust_bridge::frb;

/// Return the base58 Tari address of the wallet account named `wallet_name`.
///
/// Uses the wallet's unlocked key session (`unlock_wallet`); errors with
/// `Wallet is locked` otherwise, or if the account does not exist. `network` selects
/// the network the address is encoded for (`None` → MainNet). Synchronous.
#[frb]
pub fn get_address(wallet_name: String, network: Option<TariNetwork>) -> Result<String> {
    let network = parse_network(network);
    apply_network(network)?;
    let conn = get_db_connection()?;
    let account = unlocked_account(&conn, &wallet_name)?;
    // An empty passphrase makes minotari use the session.
    let address = account.get_address(network, "")?;

    Ok(address.to_base58())
}
