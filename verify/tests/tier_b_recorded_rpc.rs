//! Tier B — Recorded/replayed RPC for the network read paths (run on every PR).
//!
//! The base-node read APIs (`get_tip_info`, `is_node_synced`) are part of the
//! frozen public contract (ledger D2) and perform a `GET /get_tip_info`. Here we
//! stand up a local `wiremock` server that returns the **committed** recorded JSON
//! (see `verify::fixture::rpc`), point the API at it via the `base_url` parameter,
//! and assert the responses parse into the expected `TipInfo` / sync flag.
//!
//! No real base node, fully deterministic. `make record-fixtures` refreshes the
//! committed captures if the upstream RPC shape changes.

use rust_lib_flutter_rust_wallet::api::base_node::{
    check_node_health, get_tip_info, is_node_synced,
};
use rust_lib_flutter_rust_wallet::api::height::get_height_at_time;
use rust_lib_flutter_rust_wallet::api::network::{current_network, TariNetwork};
use verify::fixture::rpc;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spin up a mock base node serving the committed `get_tip_info` capture.
async fn mock_node(capture_file: &str) -> MockServer {
    let body = rpc::read_committed(capture_file).expect("read committed RPC capture");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get_tip_info"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn get_tip_info_parses_recorded_synced_response() {
    let server = mock_node(rpc::SYNCED_FILE).await;

    let tip = get_tip_info(server.uri())
        .await
        .expect("get_tip_info should parse the recorded response")
        .expect("recorded response has chain metadata");

    assert_eq!(tip.best_block_height, rpc::TIP_HEIGHT);
    assert_eq!(tip.pruning_horizon, rpc::PRUNING_HORIZON);
    assert_eq!(tip.pruned_height, rpc::PRUNED_HEIGHT);
    assert_eq!(tip.timestamp, rpc::TIP_TIMESTAMP);
    // The DTO renders the block hash as lowercase hex; the first byte is 0xAB.
    assert!(
        tip.best_block_hash.starts_with("ab"),
        "unexpected best_block_hash: {}",
        tip.best_block_hash
    );
}

#[tokio::test]
async fn is_node_synced_reads_the_synced_flag() {
    let synced_server = mock_node(rpc::SYNCED_FILE).await;
    assert!(
        is_node_synced(synced_server.uri())
            .await
            .expect("is_node_synced (synced)"),
        "synced capture must report is_synced = true"
    );

    let unsynced_server = mock_node(rpc::UNSYNCED_FILE).await;
    assert!(
        !is_node_synced(unsynced_server.uri())
            .await
            .expect("is_node_synced (unsynced)"),
        "unsynced capture must report is_synced = false"
    );
}

#[tokio::test]
async fn check_node_health_true_when_node_reachable() {
    // Reachable node: /get_tip_info answers 200, so health is true even if unsynced.
    let server = mock_node(rpc::SYNCED_FILE).await;
    assert!(
        check_node_health(server.uri())
            .await
            .expect("check_node_health must not error on a reachable node"),
        "a reachable node must report healthy = true"
    );
}

#[tokio::test]
async fn check_node_health_false_when_node_unreachable() {
    // No route mounted: requests 404 -> RPC error -> false, never Err.
    let server = MockServer::start().await;
    assert!(
        !check_node_health(server.uri())
            .await
            .expect("check_node_health must answer false, not Err, on failure"),
        "an unreachable node must report healthy = false"
    );
}

/// 2026-01-01; a mainnet node reported height 170_622 for it.
const JAN_2026: u64 = 1_767_225_600;

async fn node_answering_height(height: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get_height_at_time"))
        .and(query_param("time", JAN_2026.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(height))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn get_height_at_time_returns_the_node_height() {
    let server = node_answering_height(170_622).await;
    let height = get_height_at_time(server.uri(), JAN_2026, Some(TariNetwork::MainNet))
        .await
        .expect("get_height_at_time");
    assert_eq!(height, 170_622);
}

#[tokio::test]
async fn get_height_at_time_refuses_an_implausible_height() {
    let server = node_answering_height(175_000).await;
    let err = get_height_at_time(server.uri(), JAN_2026, Some(TariNetwork::MainNet))
        .await
        .expect_err("a height more than two days above the estimate must be refused");
    assert!(
        err.to_string()
            .starts_with("Network Error: Node reported an implausible height 175000"),
        "got {err}"
    );
}

/// The cap applies on MainNet only (`None` resolves to MainNet); other networks get
/// the node's answer unchecked.
#[tokio::test]
async fn get_height_at_time_checks_mainnet_only() {
    let server = node_answering_height(175_000).await;
    let err = get_height_at_time(server.uri(), JAN_2026, None)
        .await
        .expect_err("None is MainNet, which is checked");
    assert!(err.to_string().contains("implausible height"), "got {err}");

    let height = get_height_at_time(server.uri(), JAN_2026, Some(TariNetwork::Esmeralda))
        .await
        .expect("other networks are not checked");
    assert_eq!(height, 175_000);
}

#[tokio::test]
async fn get_height_at_time_node_error_is_network_error() {
    // No route mounted: the node answers 404.
    let server = MockServer::start().await;
    let err = get_height_at_time(server.uri(), JAN_2026, None)
        .await
        .expect_err("a failing node must error");
    assert!(err.to_string().starts_with("Network Error: "), "got {err}");
}

/// Nothing in this binary sets the process network, so it reads as unset.
#[test]
fn current_network_is_none_until_a_wallet_call_sets_it() {
    assert_eq!(current_network(), None);
}
