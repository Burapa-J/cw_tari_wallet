use crate::api::error::WalletError;
use anyhow::{Error, Result};
use std::str::FromStr;

use flutter_rust_bridge::frb;
use tari_common::configuration::Network;
use tari_common::network_check::set_network_if_choice_valid;

/// The Tari network a wallet/address/transaction targets.
///
/// Wherever a public function takes `Option<TariNetwork>`, `None` resolves to
/// `MainNet` (frozen behavior — ledger D3).
#[frb]
#[derive(Clone, PartialEq, Eq, Copy, Debug)]
pub enum TariNetwork {
    MainNet,
    StageNet,
    NextNet,
    LocalNet,
    Igor,
    Esmeralda,
}

impl FromStr for TariNetwork {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(TariNetwork::MainNet),
            "stagenet" => Ok(TariNetwork::StageNet),
            "nextnet" => Ok(TariNetwork::NextNet),
            "localnet" => Ok(TariNetwork::LocalNet),
            "igor" => Ok(TariNetwork::Igor),
            "esmeralda" | "esme" => Ok(TariNetwork::Esmeralda),
            invalid => Err(WalletError::InvalidNetwork {
                value: invalid.to_string(),
            }
            .into()),
        }
    }
}

impl From<TariNetwork> for Network {
    fn from(network: TariNetwork) -> Self {
        match network {
            TariNetwork::MainNet => Network::MainNet,
            TariNetwork::StageNet => Network::StageNet,
            TariNetwork::NextNet => Network::NextNet,
            TariNetwork::LocalNet => Network::LocalNet,
            TariNetwork::Igor => Network::Igor,
            TariNetwork::Esmeralda => Network::Esmeralda,
        }
    }
}

impl From<Network> for TariNetwork {
    fn from(network: Network) -> Self {
        match network {
            Network::MainNet => TariNetwork::MainNet,
            Network::StageNet => TariNetwork::StageNet,
            Network::NextNet => TariNetwork::NextNet,
            Network::LocalNet => TariNetwork::LocalNet,
            Network::Igor => TariNetwork::Igor,
            Network::Esmeralda => TariNetwork::Esmeralda,
        }
    }
}

/// The network this process is locked to, or `None` if no wallet call has set it yet.
///
/// The network is set once per process by the first call that takes a `network`
/// argument; a later call with a different network errors until the app restarts.
/// Compare this with the wallet's network to detect that case and prompt a restart.
#[frb(sync)]
pub fn current_network() -> Option<TariNetwork> {
    if Network::is_set() {
        Some(Network::get_current().into())
    } else {
        None
    }
}

/// Resolve an optional caller-supplied network to a concrete [`Network`].
///
/// FROZEN behavior (ledger D3, Shared Contracts §4): `None` resolves to
/// [`Network::MainNet`]. Cake Wallet depends on both this signature and the silent
/// default, so neither may change here — the fallback is merely *observable*
/// via a warning log; the resolved value is unchanged.
pub(crate) fn parse_network(network: Option<TariNetwork>) -> Network {
    match network {
        Some(n) => n.into(),
        None => {
            log::warn!("No network specified; defaulting to MainNet");
            Network::MainNet
        }
    }
}

/// The single choke-point for installing the process-global Tari network.
///
/// **Why a process-global at all (load-bearing — do not remove):** `tari_common`
/// keeps the active network in a write-once `OnceLock` (`Network::set_current`),
/// and address/consensus derivation deep inside `minotari`/`tari_*` reads it via
/// `Network::get_current()`. `set_network_if_choice_valid` (a) rejects a network
/// the binary wasn't built for and (b) is **idempotent for the same value** —
/// setting it again to the already-installed network is a no-op, while setting it
/// to a *different* one errors. Each public entry point that derives addresses or
/// builds/signs transactions therefore funnels its already-resolved [`Network`]
/// through here before touching that derived state.
///
/// Behavior is identical to the previous scattered `set_network_if_choice_valid`
/// calls — same function, same argument, same error — only centralized so the
/// constraint is documented in exactly one place. Determinism does **not** depend
/// on call order: within a process the first successful set wins and every later
/// call with the same network is a no-op (a different network is an explicit
/// error, never a silent overwrite). Per-call derivation (e.g. address building)
/// also takes `Network` as an explicit parameter, so the derived value
/// never relies on whatever was set last.
pub(crate) fn apply_network(network: Network) -> Result<()> {
    set_network_if_choice_valid(network)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Pure parsing/conversion tests. No I/O or global state — deterministic.
    //! Several of these are CONTRACT GUARDS (ledger D3): flipping them signals a
    //! breaking change to the frozen Dart-facing API.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn from_str_accepts_every_canonical_name() {
        assert_eq!(
            "mainnet".parse::<TariNetwork>().unwrap(),
            TariNetwork::MainNet
        );
        assert_eq!(
            "stagenet".parse::<TariNetwork>().unwrap(),
            TariNetwork::StageNet
        );
        assert_eq!(
            "nextnet".parse::<TariNetwork>().unwrap(),
            TariNetwork::NextNet
        );
        assert_eq!(
            "localnet".parse::<TariNetwork>().unwrap(),
            TariNetwork::LocalNet
        );
        assert_eq!("igor".parse::<TariNetwork>().unwrap(), TariNetwork::Igor);
        assert_eq!(
            "esmeralda".parse::<TariNetwork>().unwrap(),
            TariNetwork::Esmeralda
        );
    }

    #[test]
    fn from_str_esme_is_alias_for_esmeralda() {
        // CONTRACT GUARD: the "esme" alias (network.rs) must keep mapping to Esmeralda.
        assert_eq!(
            "esme".parse::<TariNetwork>().unwrap(),
            TariNetwork::Esmeralda
        );
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(
            "MainNet".parse::<TariNetwork>().unwrap(),
            TariNetwork::MainNet
        );
        assert_eq!(
            "MAINNET".parse::<TariNetwork>().unwrap(),
            TariNetwork::MainNet
        );
        assert_eq!(
            "EsMe".parse::<TariNetwork>().unwrap(),
            TariNetwork::Esmeralda
        );
        assert_eq!(
            "ESMERALDA".parse::<TariNetwork>().unwrap(),
            TariNetwork::Esmeralda
        );
    }

    #[test]
    fn from_str_rejects_unknown_names() {
        assert!("".parse::<TariNetwork>().is_err());
        assert!("bitcoin".parse::<TariNetwork>().is_err());
        assert!("main net".parse::<TariNetwork>().is_err());
    }

    #[test]
    fn from_str_error_message_is_stable_after_wallet_error_migration() {
        // BASELINE CONTRACT: the rejection message keeps the legacy
        // `"Invalid network option: {invalid}"` shape (now backed by
        // WalletError::InvalidNetwork, surfaced via anyhow at the boundary).
        let err = "bitcoin".parse::<TariNetwork>().unwrap_err();
        assert_eq!(err.to_string(), "Invalid network option: bitcoin");
    }

    #[test]
    fn tari_network_maps_to_lib_network_exhaustively() {
        // Upstream-drift tripwire: every TariNetwork variant must map to the
        // matching `tari_common::Network` variant.
        assert_eq!(Network::from(TariNetwork::MainNet), Network::MainNet);
        assert_eq!(Network::from(TariNetwork::StageNet), Network::StageNet);
        assert_eq!(Network::from(TariNetwork::NextNet), Network::NextNet);
        assert_eq!(Network::from(TariNetwork::LocalNet), Network::LocalNet);
        assert_eq!(Network::from(TariNetwork::Igor), Network::Igor);
        assert_eq!(Network::from(TariNetwork::Esmeralda), Network::Esmeralda);
    }

    #[test]
    fn lib_network_maps_back_to_tari_network() {
        for variant in [
            TariNetwork::MainNet,
            TariNetwork::StageNet,
            TariNetwork::NextNet,
            TariNetwork::LocalNet,
            TariNetwork::Igor,
            TariNetwork::Esmeralda,
        ] {
            assert_eq!(TariNetwork::from(Network::from(variant)), variant);
        }
    }

    #[test]
    fn parse_network_none_is_mainnet() {
        // CONTRACT GUARD (ledger D3): parse_network(None) -> Network::MainNet.
        // Cake Wallet depends on this silent default; changing it is breaking.
        assert_eq!(parse_network(None), Network::MainNet);
    }

    #[test]
    fn parse_network_some_round_trips_each_variant() {
        for (variant, expected) in [
            (TariNetwork::MainNet, Network::MainNet),
            (TariNetwork::StageNet, Network::StageNet),
            (TariNetwork::NextNet, Network::NextNet),
            (TariNetwork::LocalNet, Network::LocalNet),
            (TariNetwork::Igor, Network::Igor),
            (TariNetwork::Esmeralda, Network::Esmeralda),
        ] {
            assert_eq!(parse_network(Some(variant)), expected);
        }
    }
}
