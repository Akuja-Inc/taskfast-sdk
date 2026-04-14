//! Wiremock fixtures for `taskfast_agent::events`.
//!
//! Exercises cursor discipline (the generated client passes `cursor=` and
//! `limit=` query params), fast backfill (no `poll_interval` sleep when
//! `has_more=true`), and error-resumability (the stream surfaces errors
//! but stays alive for the next `.next()`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use taskfast_agent::events::{list_events_page, stream_events, PollOptions};
use taskfast_client::TaskFastClient;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

fn event(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "event": name,
        "data": { "note": "x" },
        "occurred_at": "2026-03-23T12:00:00Z",
    })
}

fn page(
    events: Vec<serde_json::Value>,
    next_cursor: Option<&str>,
    has_more: bool,
) -> serde_json::Value {
    serde_json::json!({
        "data": events,
        "meta": {
            "has_more": has_more,
            "next_cursor": next_cursor,
            "total_count": 0,
        },
    })
}

#[tokio::test]
async fn list_events_page_passes_cursor_and_limit_as_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .and(query_param("cursor", "opaque-cursor-abc"))
        .and(query_param("limit", "25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![event(
                "11111111-1111-1111-1111-111111111111",
                "task_assigned",
            )],
            None,
            false,
        )))
        .mount(&server)
        .await;

    let resp = list_events_page(&client(&server), Some("opaque-cursor-abc"), Some(25))
        .await
        .expect("happy path");
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].event, "task_assigned");
}

/// Sequence-of-responses Respond impl (same shape used in the wallet +
/// client tests). Saturates at the last entry so long-running polls
/// don't index out of bounds when they outlast the script.
struct Sequence {
    responses: Vec<ResponseTemplate>,
    cursor: Arc<AtomicUsize>,
}
impl Sequence {
    fn new(responses: Vec<ResponseTemplate>) -> Self {
        Self {
            responses,
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Respond for Sequence {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let idx = self
            .cursor
            .fetch_add(1, Ordering::SeqCst)
            .min(self.responses.len() - 1);
        self.responses[idx].clone()
    }
}

#[tokio::test]
async fn stream_drains_multi_page_backlog_without_sleeping() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(200).set_body_json(page(
                vec![
                    event("11111111-1111-1111-1111-111111111111", "ev_a"),
                    event("22222222-2222-2222-2222-222222222222", "ev_b"),
                ],
                Some("cursor-1"),
                true,
            )),
            ResponseTemplate::new(200).set_body_json(page(
                vec![event("33333333-3333-3333-3333-333333333333", "ev_c")],
                Some("cursor-2"),
                false,
            )),
        ]))
        .mount(&server)
        .await;

    // Deliberately oversized poll_interval: if the stream incorrectly
    // sleeps between pages (ignoring has_more=true), this test would
    // take 5s instead of ~0ms and trip the elapsed-time assertion.
    let opts = PollOptions {
        poll_interval: Duration::from_secs(5),
        page_limit: 100,
    };
    let c = client(&server);
    let mut stream = Box::pin(stream_events(&c, None, opts));

    let started = Instant::now();
    let first = stream.next().await.expect("ev_a").expect("ok");
    let second = stream.next().await.expect("ev_b").expect("ok");
    let third = stream.next().await.expect("ev_c").expect("ok");
    let elapsed = started.elapsed();

    assert_eq!(first.event, "ev_a");
    assert_eq!(second.event, "ev_b");
    assert_eq!(third.event, "ev_c");
    assert!(
        elapsed < Duration::from_millis(500),
        "backfill should skip sleep; took {elapsed:?}"
    );
}

#[tokio::test]
async fn stream_sleeps_at_page_tip_then_resumes_with_last_cursor() {
    let server = MockServer::start().await;

    // Turn 1: cursor=None → one event, next_cursor=c1, has_more=false (tip).
    // Turn 2: cursor=c1 → new event, no more.
    // query_param matchers make the cursor discipline an executable spec.
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .and(wiremock::matchers::query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![event("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "first")],
            Some("c1"),
            false,
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .and(query_param("cursor", "c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![event("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "second")],
            Some("c2"),
            false,
        )))
        .mount(&server)
        .await;

    let opts = PollOptions {
        poll_interval: Duration::from_millis(50),
        page_limit: 100,
    };
    let c = client(&server);
    let mut stream = Box::pin(stream_events(&c, None, opts));

    let first = stream.next().await.unwrap().expect("first");
    assert_eq!(first.event, "first");
    let second = stream.next().await.unwrap().expect("second");
    assert_eq!(second.event, "second");
}

#[tokio::test]
async fn stream_surfaces_errors_without_terminating() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/events"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": "internal", "message": "boom",
            })),
            ResponseTemplate::new(200).set_body_json(page(
                vec![event("cccccccc-cccc-cccc-cccc-cccccccccccc", "recovered")],
                None,
                false,
            )),
        ]))
        .mount(&server)
        .await;

    let opts = PollOptions {
        poll_interval: Duration::from_millis(20),
        page_limit: 100,
    };
    let c = client(&server);
    let mut stream = Box::pin(stream_events(&c, None, opts));

    // First yield: the 500 bubbles up, but the stream is still alive.
    match stream.next().await {
        Some(Err(_)) => {}
        other => panic!("expected Err, got {other:?}"),
    }
    // Second yield: next fetch succeeds and we recover.
    let recovered = stream.next().await.unwrap().expect("ok");
    assert_eq!(recovered.event, "recovered");
}
