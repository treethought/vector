use crate::{
    codecs::{self, BytesCodec, BytesParser},
    config::{log_schema, SourceConfig, SourceContext},
    event::{Event, EventStatus},
    sources::datadog::{
        DatadogAgentConfig,
        logs::{decode_log_body, LogMsg},
    },
    test_util::{next_addr, spawn_collect_n, trace_init, wait_for_tcp},
    Pipeline,
};
use bytes::Bytes;
use futures::Stream;
use http::HeaderMap;
use indoc::indoc;
use pretty_assertions::assert_eq;
use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
use std::net::SocketAddr;

impl Arbitrary for LogMsg {
    fn arbitrary(g: &mut Gen) -> Self {
        LogMsg {
            message: Bytes::from(String::arbitrary(g)),
            status: Bytes::from(String::arbitrary(g)),
            timestamp: i64::arbitrary(g),
            hostname: Bytes::from(String::arbitrary(g)),
            service: Bytes::from(String::arbitrary(g)),
            ddsource: Bytes::from(String::arbitrary(g)),
            ddtags: Bytes::from(String::arbitrary(g)),
        }
    }
}

// We want to know that for any json payload that is a `Vec<LogMsg>` we can
// correctly decode it into a `Vec<LogEvent>`. For convenience we assume
// that order is preserved in the decoding step though this is not
// necessarily part of the contract of that function.
#[test]
fn test_decode_body() {
    fn inner(msgs: Vec<LogMsg>) -> TestResult {
        let body = Bytes::from(serde_json::to_string(&msgs).unwrap());
        let api_key = None;

        let decoder =
            codecs::Decoder::new(Box::new(BytesCodec::new()), Box::new(BytesParser::new()));
        let events = decode_log_body(body, api_key, decoder).unwrap();
        assert_eq!(events.len(), msgs.len());
        for (msg, event) in msgs.into_iter().zip(events.into_iter()) {
            let log = event.as_log();
            assert_eq!(log["message"], msg.message.into());
            assert_eq!(log["status"], msg.status.into());
            assert_eq!(log["timestamp"], msg.timestamp.into());
            assert_eq!(log["hostname"], msg.hostname.into());
            assert_eq!(log["service"], msg.service.into());
            assert_eq!(log["ddsource"], msg.ddsource.into());
            assert_eq!(log["ddtags"], msg.ddtags.into());
        }

        TestResult::passed()
    }

    QuickCheck::new().quickcheck(inner as fn(Vec<LogMsg>) -> TestResult);
}

#[test]
fn generate_config() {
    crate::test_util::test_generate_config::<DatadogAgentConfig>();
}

async fn source(
    status: EventStatus,
    acknowledgements: bool,
    store_api_key: bool,
) -> (impl Stream<Item = Event>, SocketAddr) {
    let (sender, recv) = Pipeline::new_test_finalize(status);
    let address = next_addr();
    let context = SourceContext::new_test(sender);
    let config = toml::from_str::<DatadogAgentConfig>(&format!(
        indoc! { r#"
            address = "{}"
            compression = "none"
            store_api_key = {}
            acknowledgements = {}
        "#},
        address, store_api_key, acknowledgements
    ))
    .unwrap();

    tokio::spawn(async move {
        config.build(context).await.unwrap().await.unwrap();
    });
    wait_for_tcp(address).await;
    (recv, address)
}

async fn send_with_path(address: SocketAddr, body: &str, headers: HeaderMap, path: &str) -> u16 {
    reqwest::Client::new()
        .post(&format!("http://{}{}", address, path))
        .headers(headers)
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn full_payload_v1() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("foo"),
                        timestamp: 123,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/v1/input/"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "foo".into());
        assert_eq!(log["timestamp"], 123.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert!(event.metadata().datadog_api_key().is_none());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
    }
}

#[tokio::test]
async fn full_payload_v2() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("foo"),
                        timestamp: 123,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/api/v2/logs"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "foo".into());
        assert_eq!(log["timestamp"], 123.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert!(event.metadata().datadog_api_key().is_none());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
    }
}

#[tokio::test]
async fn no_api_key() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("foo"),
                        timestamp: 123,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/v1/input/"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "foo".into());
        assert_eq!(log["timestamp"], 123.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert!(event.metadata().datadog_api_key().is_none());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
    }
}

#[tokio::test]
async fn api_key_in_url() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("bar"),
                        timestamp: 456,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/v1/input/12345678abcdefgh12345678abcdefgh"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "bar".into());
        assert_eq!(log["timestamp"], 456.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
        assert_eq!(
            &event.metadata().datadog_api_key().as_ref().unwrap()[..],
            "12345678abcdefgh12345678abcdefgh"
        );
    }
}

#[tokio::test]
async fn api_key_in_query_params() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("bar"),
                        timestamp: 456,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/api/v2/logs?dd-api-key=12345678abcdefgh12345678abcdefgh"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "bar".into());
        assert_eq!(log["timestamp"], 456.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
        assert_eq!(
            &event.metadata().datadog_api_key().as_ref().unwrap()[..],
            "12345678abcdefgh12345678abcdefgh"
        );
    }
}

#[tokio::test]
async fn api_key_in_header() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, true).await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "dd-api-key",
        "12345678abcdefgh12345678abcdefgh".parse().unwrap(),
    );

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("baz"),
                        timestamp: 789,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    headers,
                    "/v1/input/"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "baz".into());
        assert_eq!(log["timestamp"], 789.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
        assert_eq!(
            &event.metadata().datadog_api_key().as_ref().unwrap()[..],
            "12345678abcdefgh12345678abcdefgh"
        );
    }
}

#[tokio::test]
async fn delivery_failure() {
    trace_init();
    let (rx, addr) = source(EventStatus::Failed, true, true).await;

    spawn_collect_n(
        async move {
            assert_eq!(
                400,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("foo"),
                        timestamp: 123,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/v1/input/"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;
}

#[tokio::test]
async fn ignores_disabled_acknowledgements() {
    trace_init();
    let (rx, addr) = source(EventStatus::Failed, false, true).await;

    let events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("foo"),
                        timestamp: 123,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    HeaderMap::new(),
                    "/v1/input/"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn ignores_api_key() {
    trace_init();
    let (rx, addr) = source(EventStatus::Delivered, true, false).await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "dd-api-key",
        "12345678abcdefgh12345678abcdefgh".parse().unwrap(),
    );

    let mut events = spawn_collect_n(
        async move {
            assert_eq!(
                200,
                send_with_path(
                    addr,
                    &serde_json::to_string(&[LogMsg {
                        message: Bytes::from("baz"),
                        timestamp: 789,
                        hostname: Bytes::from("festeburg"),
                        status: Bytes::from("notice"),
                        service: Bytes::from("vector"),
                        ddsource: Bytes::from("curl"),
                        ddtags: Bytes::from("one,two,three"),
                    }])
                    .unwrap(),
                    headers,
                    "/v1/input/12345678abcdefgh12345678abcdefgh"
                )
                .await
            );
        },
        rx,
        1,
    )
    .await;

    {
        let event = events.remove(0);
        let log = event.as_log();
        assert_eq!(log["message"], "baz".into());
        assert_eq!(log["timestamp"], 789.into());
        assert_eq!(log["hostname"], "festeburg".into());
        assert_eq!(log["status"], "notice".into());
        assert_eq!(log["service"], "vector".into());
        assert_eq!(log["ddsource"], "curl".into());
        assert_eq!(log["ddtags"], "one,two,three".into());
        assert_eq!(log[log_schema().source_type_key()], "datadog_agent".into());
        assert!(event.metadata().datadog_api_key().is_none());
    }
}
