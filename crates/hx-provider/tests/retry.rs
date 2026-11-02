use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use hx_provider::*;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn transient() -> ProviderError {
    ProviderError::Transient { status: Some(529), retry_after: None, msg: "overloaded".into() }
}
fn text(s: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(s.into())
}
fn done() -> ProviderEvent {
    ProviderEvent::Done { stop: StopReason::EndTurn }
}
fn req() -> CanonicalRequest {
    CanonicalRequest {
        model: "mock/m".into(),
        system: vec![],
        tools: vec![],
        messages: vec![],
        max_output: 100,
        reasoning: None,
        cache_breakpoints: vec![],
    }
}
async fn collect(
    s: BoxStream<'static, Result<ProviderEvent, ProviderError>>,
) -> Vec<Result<ProviderEvent, ProviderError>> {
    s.collect().await
}

#[tokio::test(start_paused = true)]
async fn retries_transient_then_succeeds() {
    let m = MockProvider::new(vec![
        MockTurn::Error(transient()),
        MockTurn::Error(transient()),
        MockTurn::Events(vec![text("ok"), done()]),
    ]);
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![Ok(text("ok")), Ok(done())]);
    assert_eq!(m.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn honors_retry_after() {
    let e = ProviderError::Transient { status: Some(429), retry_after: Some(Duration::from_secs(7)), msg: "slow".into() };
    let m = MockProvider::new(vec![MockTurn::Error(e), MockTurn::Events(vec![done()])]);
    let t0 = Instant::now();
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![Ok(done())]);
    assert!(t0.elapsed() >= Duration::from_secs(7));
    assert!(t0.elapsed() < Duration::from_secs(8), "retry_after is used exactly, no jitter");
}

#[tokio::test(start_paused = true)]
async fn backoff_grows_exponentially_with_bounded_jitter() {
    let m = MockProvider::new(vec![MockTurn::Error(transient()), MockTurn::Error(transient()), MockTurn::Events(vec![done()])]);
    let t0 = Instant::now();
    collect(Retrying::new(m).stream(req(), CancellationToken::new())).await;
    // 500ms*[0.75,1.25] + 1000ms*[0.75,1.25]
    let el = t0.elapsed();
    assert!(el >= Duration::from_millis(1125) && el <= Duration::from_millis(1875), "{el:?}");
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_five_attempts() {
    let m = MockProvider::new((0..6).map(|_| MockTurn::Error(transient())).collect());
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![Err(transient())]);
    assert_eq!(m.requests().len(), 5);
}

#[tokio::test]
async fn auth_error_not_retried() {
    let m = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("bad".into())), MockTurn::Events(vec![done()])]);
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![Err(ProviderError::Auth("bad".into()))]);
    assert_eq!(m.requests().len(), 1);
}

#[tokio::test]
async fn mid_stream_drop_surfaces_error_without_retry() {
    let m = MockProvider::new(vec![MockTurn::DropAfter(vec![text("a")]), MockTurn::Events(vec![done()])]);
    let out = collect(Retrying::new(m.clone()).stream(req(), CancellationToken::new())).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], Ok(text("a")));
    assert!(matches!(out[1], Err(ProviderError::Transient { .. })));
    assert_eq!(m.requests().len(), 1);
}

#[tokio::test]
async fn cancel_stops_stall() {
    let m = MockProvider::new(vec![MockTurn::Stall]);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        c2.cancel();
    });
    let t0 = std::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_millis(50), collect(Retrying::new(m).stream(req(), cancel)))
        .await
        .expect("stream must end promptly after cancel");
    assert!(out.is_empty());
    assert!(t0.elapsed() < Duration::from_millis(50));
}

#[tokio::test(start_paused = true)]
async fn cancel_interrupts_backoff_and_stops_attempts() {
    let e = ProviderError::Transient { status: None, retry_after: Some(Duration::from_secs(60)), msg: "x".into() };
    let m = MockProvider::new(vec![MockTurn::Error(e), MockTurn::Events(vec![done()])]);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        c2.cancel();
    });
    let t0 = Instant::now();
    let out = collect(Retrying::new(m.clone()).stream(req(), cancel)).await;
    assert!(out.is_empty());
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(m.requests().len(), 1);
}

#[tokio::test]
async fn mock_script_exhausted_and_names() {
    let m = MockProvider::new(vec![]);
    assert_eq!(m.name(), "mock");
    assert_eq!(Retrying::new(m.clone()).name(), "mock");
    let out = collect(m.stream(req(), CancellationToken::new())).await;
    assert_eq!(out, vec![Err(ProviderError::Other("mock script exhausted".into()))]);
}
