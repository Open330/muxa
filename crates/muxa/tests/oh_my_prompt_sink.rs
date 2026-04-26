//! Integration tests for the oh-my-prompt sink.
//!
//! These spin up a `wiremock` HTTP server, point a real
//! `OhMyPromptSink` at it (via `resolve` + a hermetic env var per
//! test), and drive the sink with `tokio::sync::broadcast` channels —
//! same shape as the live wire-up in `muxad`.
//!
//! Time-sensitive tests use `tokio::time::pause` / `advance` so they
//! complete in milliseconds rather than waiting on real wall-clock
//! flush intervals.

use muxa::config::OhMyPromptToml;
use muxa::event::{AgentId, AgentKind};
use muxa::sinks::OhMyPromptSink;
use muxa::state::PromptRecord;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Each test gets its own env var name so concurrent test runs don't
/// stomp on each other (Rust's test runner uses one process for all
/// tests in a binary, so env-var state is shared).
fn unique_token_var() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("OMP_TEST_TOKEN_{n}")
}

fn make_prompt(prompt: &str, session: &str) -> PromptRecord {
    PromptRecord {
        id: AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            pane: Some("%1".into()),
            cwd: Some("/work/proj".into()),
        },
        prompt: prompt.into(),
        at: OffsetDateTime::now_utc(),
        model: Some("opus".into()),
    }
}

/// Build a TOML config pointing at `endpoint`, with a unique token var
/// preloaded with `token_value`. Returns the resolved sink.
fn build_sink(
    endpoint: &str,
    token_value: &str,
    batch_size: usize,
    flush_interval_ms: u64,
) -> OhMyPromptSink {
    let var = unique_token_var();
    // SAFETY: the env-var name is unique per test invocation, so this
    // doesn't race with parallel tests.
    std::env::set_var(&var, token_value);
    let toml = OhMyPromptToml {
        enabled: Some(true),
        endpoint: Some(endpoint.to_string()),
        token_env: Some(var),
        device_id: Some("test-device".into()),
        batch_size: Some(batch_size),
        flush_interval_ms: Some(flush_interval_ms),
    };
    OhMyPromptSink::resolve(&toml)
        .expect("resolve must succeed")
        .expect("sink must be Some when enabled")
}

#[tokio::test]
async fn posts_to_correct_endpoint_with_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .and(header("X-User-Token", "the-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(&server)
        .await;

    let sink = build_sink(&server.uri(), "the-token", 1, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(8);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    prompt_tx.send(make_prompt("hello", "sess-1")).unwrap();

    // batch_size=1 means the first prompt triggers an immediate flush.
    // Wait briefly for the HTTP call to register, then shut down.
    for _ in 0..50 {
        if !server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one HTTP call");
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let records = body.get("records").and_then(Value::as_array).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].get("prompt_text").and_then(Value::as_str),
        Some("hello")
    );
    assert_eq!(
        records[0].get("source").and_then(Value::as_str),
        Some("claude-code")
    );
    assert_eq!(records[0].get("role").and_then(Value::as_str), Some("user"));
    assert_eq!(
        body.get("deviceId").and_then(Value::as_str),
        Some("test-device")
    );
}

#[tokio::test]
async fn batches_records_at_size_threshold() {
    let server = MockServer::start().await;
    // batch_size=50, time threshold high — only one POST should land
    // when we feed exactly 50 prompts.
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let sink = build_sink(&server.uri(), "tok", 50, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(128);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    for i in 0..50 {
        prompt_tx
            .send(make_prompt(&format!("prompt-{i}"), &format!("s-{i}")))
            .unwrap();
    }

    // Wait for the batch to drain.
    for _ in 0..100 {
        let n = server.received_requests().await.map_or(0, |r| r.len());
        if n >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one batch POST");
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let records = body.get("records").and_then(Value::as_array).unwrap();
    assert_eq!(records.len(), 50);
}

#[tokio::test(start_paused = true)]
async fn flushes_records_at_time_threshold() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    // batch_size large; flush by time only.
    let sink = build_sink(&server.uri(), "tok", 1000, 5_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(8);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    // Yield so the spawned task gets to its select! on a paused clock.
    tokio::task::yield_now().await;

    prompt_tx.send(make_prompt("once", "s-1")).unwrap();

    // Advance clock past the flush interval.
    tokio::time::advance(Duration::from_secs(6)).await;

    // Allow the now-due tick to drive the HTTP call. The wiremock
    // server runs on a *real* clock, so we yield to it via real-time
    // sleeps after un-pausing the tokio time? — wiremock's hyper
    // listener also uses tokio time. We need to resume real time for
    // the network round-trip. The simplest path: use `resume()`
    // after triggering the flush.
    tokio::time::resume();

    for _ in 0..100 {
        let n = server.received_requests().await.map_or(0, |r| r.len());
        if n >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "expected one POST after time-based flush"
    );
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let records = body.get("records").and_then(Value::as_array).unwrap();
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn retries_5xx_with_backoff() {
    let server = MockServer::start().await;

    // First call: 503. Second call: 200. With `up_to_n_times` we
    // sequence the responses.
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let sink = build_sink(&server.uri(), "tok", 1, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(8);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    prompt_tx.send(make_prompt("hi", "s-1")).unwrap();

    // First attempt + 500ms backoff + second attempt. Allow plenty of
    // wall-clock for both.
    for _ in 0..200 {
        let n = server.received_requests().await.map_or(0, |r| r.len());
        if n >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert!(
        received.len() >= 2,
        "expected at least two attempts (got {})",
        received.len()
    );
}

#[tokio::test]
async fn honors_retry_after_on_429() {
    let server = MockServer::start().await;

    // First response: 429 with Retry-After: 1. Second: 200.
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let sink = build_sink(&server.uri(), "tok", 1, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(8);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    prompt_tx.send(make_prompt("hi", "s-1")).unwrap();

    let start = std::time::Instant::now();
    for _ in 0..400 {
        let n = server.received_requests().await.map_or(0, |r| r.len());
        if n >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let elapsed = start.elapsed();

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert!(
        received.len() >= 2,
        "expected retry after 429 (got {})",
        received.len()
    );
    // The Retry-After of 1 second should have introduced at least
    // ~0.8s of delay between the two attempts. We're lenient so CI
    // jitter doesn't false-fail.
    assert!(
        elapsed >= Duration::from_millis(800),
        "expected ~1s delay from Retry-After, got {elapsed:?}"
    );
}

#[tokio::test]
async fn drops_4xx_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/sync/upload"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad schema"))
        .mount(&server)
        .await;

    let sink = build_sink(&server.uri(), "tok", 1, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(8);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    prompt_tx.send(make_prompt("hi", "s-1")).unwrap();

    // Wait long enough that any retry would have hit (backoff[0]=500ms).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let _ = shutdown_tx.send(());
    let _ = handle.await;

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "4xx must not trigger a retry (got {} attempts)",
        received.len()
    );
}

/// A `tracing` `Layer` that stringifies each event into a shared
/// buffer. Used by `ring_buffer_drops_oldest_on_overflow` to assert
/// the sink emits the expected WARN line — and isolated up here so
/// `clippy::items_after_statements` stays happy.
#[derive(Clone, Default)]
struct CapturedLines(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct CapturedVisitor<'a>(&'a mut String);

impl tracing::field::Visit for CapturedVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLines {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().level() > &tracing::Level::WARN {
            return;
        }
        let mut s = String::new();
        let mut v = CapturedVisitor(&mut s);
        event.record(&mut v);
        self.0.lock().unwrap().push(s);
    }
}

#[tokio::test]
async fn ring_buffer_drops_oldest_on_overflow() {
    // We point at an unreachable port so the sink can't drain its
    // buffer — every POST will fail and retry. With batch_size=1 and
    // a non-listening port the buffer fills up and starts dropping.
    //
    // We can't easily inspect the ring's contents from outside the
    // sink, so this test is a smoke test: we feed >1000 records and
    // assert the sink doesn't crash and the first-pushed records are
    // visibly absent in a tracing-captured WARN line.

    use tracing_subscriber::layer::SubscriberExt;

    let captured = CapturedLines::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    // Use a port that nothing is listening on.
    let endpoint = "http://127.0.0.1:1"; // port 1 — reserved/closed
    let sink = build_sink(endpoint, "tok", 1, 60_000);

    let (prompt_tx, prompt_rx) = broadcast::channel(2048);
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let handle = tokio::spawn(sink.run(prompt_rx, shutdown_rx));

    // Push 1100 records — buffer caps at 1000, so 100 should be dropped.
    for i in 0..1100 {
        prompt_tx
            .send(make_prompt(&format!("p-{i}"), &format!("s-{i}")))
            .unwrap();
    }
    // Brief yield to let the sink task absorb them. The HTTP attempts
    // will all fail/timeout, but pushes are sync wrt the broadcast.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = shutdown_tx.send(());
    // Don't await the handle — the task may still be spinning on retries.
    drop(handle);

    let lines = captured.0.lock().unwrap().clone();
    let dropped = lines
        .iter()
        .filter(|l| l.contains("ring buffer full") || l.contains("dropped oldest"))
        .count();
    assert!(
        dropped > 0,
        "expected at least one ring-buffer overflow WARN; got lines: {lines:?}"
    );
}
