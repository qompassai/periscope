//! Broker tests against the fake editor (see `common`).
//!
//! Validation: handshake, argument/result conversion.
//! Adversarial: editor errors map to typed errors, a silent editor
//! times out, a dying editor fails closed instead of hanging.

mod common;

use common::{Behavior, config_for, spawn_fake_editor, temp_socket};
use periscope::broker::{BrokerMode, NvimBroker};
use periscope::codec;
use periscope::error::Error;
use serde_json::json;

#[tokio::test]
async fn handshake_and_lua_roundtrip_preserve_values() {
    let socket = temp_socket("roundtrip");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let broker = NvimBroker::connect(config_for(socket))
        .await
        .expect("connect");
    assert_eq!(broker.mode(), BrokerMode::Attached);
    let args = vec![
        json!("text"),
        json!(42),
        json!(true),
        json!(null),
        json!({"nested": [1, 2, 3]}),
    ];
    let result = broker
        .exec_lua("return ...", args.clone(), 5_000)
        .await
        .expect("exec_lua");
    assert_eq!(result, json!({"echo": args}));
    broker.shutdown().await;
}

#[tokio::test]
async fn editor_error_maps_to_broker_rpc() {
    let socket = temp_socket("rpcerror");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let broker = NvimBroker::connect(config_for(socket))
        .await
        .expect("connect");
    let error = broker
        .exec_lua("BOOM", Vec::new(), 5_000)
        .await
        .expect_err("the editor error must surface");
    match error {
        Error::BrokerRpc(message) => assert!(message.contains("lua exploded")),
        other => panic!("expected BrokerRpc, got {other}"),
    }
    broker.shutdown().await;
}

#[tokio::test]
async fn silent_editor_times_out_instead_of_hanging() {
    let socket = temp_socket("silent");
    spawn_fake_editor(socket.clone(), Behavior::SilentAfterHandshake);
    let broker = NvimBroker::connect(config_for(socket))
        .await
        .expect("connect");
    let error = broker
        .exec_lua("return 1", Vec::new(), 300)
        .await
        .expect_err("a silent editor must time out");
    assert!(matches!(error, Error::Timeout { .. }), "got {error}");
    broker.shutdown().await;
}

#[tokio::test]
async fn dying_editor_fails_closed() {
    let socket = temp_socket("dying");
    spawn_fake_editor(socket.clone(), Behavior::DieAfterHandshake);
    let broker = NvimBroker::connect(config_for(socket))
        .await
        .expect("connect");
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        broker.exec_lua("return 1", Vec::new(), 4_000),
    )
    .await;
    let error = outcome
        .expect("the request must resolve, not hang")
        .expect_err("a dead editor must fail the request");
    assert!(
        matches!(error, Error::BrokerClosed | Error::Io { .. }),
        "got {error}"
    );
    broker.shutdown().await;
}

#[test]
fn codec_roundtrips_nested_values() {
    let original = json!({
        "ints": [0, -1, 9007199254740991_i64],
        "nested": {"a": {"b": [true, false, null]}},
        "text": "héllo ✦ world",
        "float": 2.5,
    });
    let encoded = codec::json_to_mp(&original);
    let decoded = codec::mp_to_json(&encoded);
    assert_eq!(decoded, original);
}
