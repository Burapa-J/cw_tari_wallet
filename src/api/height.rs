//! Conversions between dates, block heights and the wallet birthday.
//!
//! Thin adapter over `minotari::utils::height`, so Dart never computes heights or
//! days-since-genesis itself.

use crate::api::error::WalletError;
use crate::api::network::{parse_network, TariNetwork};
use crate::domain::text::strip_format_chars;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::utils::height;
use tari_common::configuration::Network;

/// Ask the base node at `base_url` for the height of the block mined at `epoch_secs`
/// (seconds since the Unix epoch) on `network` (`None` → MainNet).
///
/// On MainNet the answer is checked against [`estimate_height_at_time`]: a height
/// more than two days of blocks above it is refused. Other networks get the node's
/// answer unchecked: their block times swing too much for a tight bound and their
/// coins have no value. The estimate itself runs high (real blocks
/// are slightly slower than the target; about 17 days of blocks high by September
/// 2026, growing over time), so a hostile node can still push a restore or rescan
/// start that far plus two days late, but no further. Async; performs network I/O.
/// Errors as `Network Error: …` if the URL is invalid, the node cannot answer, or its
/// answer is implausible.
#[frb]
pub async fn get_height_at_time(
    base_url: String,
    epoch_secs: u64,
    network: Option<TariNetwork>,
) -> Result<u64> {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return Err(WalletError::network("Base node URL must not be empty").into());
    }
    let lower_url = base_url.to_ascii_lowercase();
    if !lower_url.starts_with("http://") && !lower_url.starts_with("https://") {
        return Err(
            WalletError::network("Base node URL must start with http:// or https://").into(),
        );
    }
    let node_height = height::height_at_time(base_url, epoch_secs)
        .await
        .map_err(|e| WalletError::network(strip_format_chars(&e.to_string())))?;
    let network = parse_network(network);
    if network != Network::MainNet {
        return Ok(node_height);
    }
    let estimate = height::estimate_height_at_time(network, epoch_secs);
    let max_plausible = max_plausible_height(estimate);
    if node_height > max_plausible {
        return Err(WalletError::network(format!(
            "Node reported an implausible height {node_height} for that time; expected at most {max_plausible}"
        ))
        .into());
    }
    Ok(node_height)
}

/// The highest node answer accepted for an offline `estimate`: two days of blocks
/// above it. Early mainnet blocks ran slightly faster than the target (54 blocks
/// ahead of the estimate on 2025-07-01); upstream bounds that at about a day.
fn max_plausible_height(estimate: u64) -> u64 {
    const MARGIN_BLOCKS: u64 = 2 * 24 * 60 * 60 / height::TARGET_BLOCK_TIME_SECS;
    estimate.saturating_add(MARGIN_BLOCKS)
}

/// Estimate, without a node, the height of the block mined at `epoch_secs` on
/// `network` (`None` → MainNet).
///
/// Assumes the target block time, so it tends to run a few percent above the real
/// height; subtract a margin before using it as a restore or rescan start.
#[frb(sync)]
pub fn estimate_height_at_time(network: Option<TariNetwork>, epoch_secs: u64) -> u64 {
    height::estimate_height_at_time(parse_network(network), epoch_secs)
}

/// The wallet birthday (days since the birthday genesis) for a wallet whose first
/// block is `height` on `network` (`None` → MainNet). Pass it as `birthday` to
/// [`import_wallet_from_keys`](crate::api::wallet::import_wallet_from_keys) or
/// [`import_view_only_wallet`](crate::api::wallet::import_view_only_wallet).
#[frb(sync)]
pub fn birthday_from_height(network: Option<TariNetwork>, height: u64) -> u16 {
    height::birthday_from_height(parse_network(network), height)
}

/// The wallet birthday (days since the birthday genesis) for a wallet created at
/// `epoch_secs` (seconds since the Unix epoch).
#[frb(sync)]
pub fn birthday_from_date(epoch_secs: u64) -> u16 {
    height::birthday_from_time(epoch_secs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const MAINNET_GENESIS: u64 = 1_746_511_200;
    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn estimate_height_counts_two_minute_blocks_since_genesis() {
        assert_eq!(estimate_height_at_time(None, 0), 0);
        assert_eq!(
            estimate_height_at_time(Some(TariNetwork::MainNet), MAINNET_GENESIS + DAY),
            720
        );
    }

    #[test]
    fn birthday_from_date_and_height_agree() {
        let at_genesis = birthday_from_date(MAINNET_GENESIS);
        assert_eq!(
            birthday_from_height(Some(TariNetwork::MainNet), 0),
            at_genesis
        );
        assert_eq!(
            birthday_from_height(Some(TariNetwork::MainNet), 720 * 10),
            at_genesis + 10
        );
        assert_eq!(birthday_from_date(u64::MAX), u16::MAX);
    }

    #[test]
    fn plausible_height_allows_two_days() {
        assert_eq!(max_plausible_height(0), 1_440);
        assert_eq!(max_plausible_height(100_000), 101_440);
        assert_eq!(max_plausible_height(u64::MAX), u64::MAX);
    }

    /// Real mainnet heights (from minotari's own tests) all pass the check.
    #[test]
    fn real_mainnet_heights_are_plausible() {
        for (time, node_height) in [
            (1_751_328_000u64, 40_194u64),
            (1_767_225_600, 170_622),
            (1_788_220_800, 335_339),
        ] {
            let estimate = estimate_height_at_time(Some(TariNetwork::MainNet), time);
            assert!(node_height <= max_plausible_height(estimate), "{time}");
        }
    }

    #[tokio::test]
    async fn get_height_at_time_bad_url_is_network_error() {
        let err = get_height_at_time("not a url".to_string(), 0, None)
            .await
            .expect_err("an invalid URL must error");
        assert!(err.to_string().starts_with("Network Error: "), "got: {err}");

        let empty_err = get_height_at_time("   ".to_string(), 0, None)
            .await
            .expect_err("an empty/whitespace URL must error");
        assert_eq!(
            empty_err.to_string(),
            "Network Error: Base node URL must not be empty"
        );

        let scheme_err = get_height_at_time("ftp://example.com".to_string(), 0, None)
            .await
            .expect_err("a non-HTTP(S) URL must error");
        assert_eq!(
            scheme_err.to_string(),
            "Network Error: Base node URL must start with http:// or https://"
        );
    }

    #[tokio::test]
    async fn get_height_at_time_enforces_mainnet_plausibility_and_trims_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\nConnection: close\r\n\r\n999999999";
                    let _ = socket.write_all(resp).await;
                }
            }
        });

        let padded_url = format!("  http://{addr}  ");
        let mainnet_err = get_height_at_time(
            padded_url.clone(),
            MAINNET_GENESIS + DAY,
            Some(TariNetwork::MainNet),
        )
        .await
        .expect_err("implausible MainNet height must be rejected");
        assert!(
            mainnet_err
                .to_string()
                .contains("Node reported an implausible height 999999999"),
            "got: {mainnet_err}"
        );

        let esme_height = get_height_at_time(
            padded_url,
            MAINNET_GENESIS + DAY,
            Some(TariNetwork::Esmeralda),
        )
        .await
        .expect("non-MainNet accepts node answer");
        assert_eq!(esme_height, 999_999_999);
    }
}
