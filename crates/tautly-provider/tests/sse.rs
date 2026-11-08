use bytes::Bytes;
use futures::StreamExt;
use tautly_provider::sse::{SseEvent, events};

async fn parse(chunks: &[&str]) -> Vec<SseEvent> {
    let body = futures::stream::iter(
        chunks
            .iter()
            .map(|c| Bytes::from(c.to_string()))
            .collect::<Vec<_>>(),
    );
    events(body).map(|r| r.unwrap()).collect().await
}

fn ev(event: Option<&str>, data: &str) -> SseEvent {
    SseEvent {
        event: event.map(String::from),
        data: data.to_string(),
    }
}

#[tokio::test]
async fn parses_lf_and_crlf() {
    let got = parse(&["event: a\ndata: 1\n\nevent: b\r\ndata: 2\r\n\r\n"]).await;
    assert_eq!(got, vec![ev(Some("a"), "1"), ev(Some("b"), "2")]);
}

#[tokio::test]
async fn joins_multiline_data_and_skips_comments() {
    let got = parse(&[": keep-alive\ndata: line1\ndata: line2\n\n"]).await;
    assert_eq!(got, vec![ev(None, "line1\nline2")]);
}

#[tokio::test]
async fn handles_split_chunks_even_mid_crlf_and_mid_utf8() {
    let got = parse(&[
        "event: x\r",
        "\ndata: h",
        "ello\r\n\r",
        "\ndata: caf",
        "\n\n",
    ])
    .await;
    assert_eq!(got, vec![ev(Some("x"), "hello"), ev(None, "caf")]);

    let bytes = "data: é\n\n".as_bytes().to_vec();
    let body = futures::stream::iter(vec![
        Bytes::from(bytes[..7].to_vec()),
        Bytes::from(bytes[7..].to_vec()),
    ]);
    let got: Vec<_> = events(body).map(|r| r.unwrap()).collect().await;
    assert_eq!(got, vec![ev(None, "é")]);
}

#[tokio::test]
async fn flushes_final_event_without_blank_line() {
    let got = parse(&["data: a\n\ndata: tail"]).await;
    assert_eq!(got, vec![ev(None, "a"), ev(None, "tail")]);
}

#[tokio::test]
async fn empty_data_blocks_are_not_dispatched() {
    let got = parse(&[": only a comment\n\n\n\ndata: x\n\n"]).await;
    assert_eq!(got, vec![ev(None, "x")]);
}

/// Review M5: a line that never ends (or one event's data growing without a blank line)
/// is cut off at 16 MiB as a transient error instead of growing without limit.
#[tokio::test]
async fn an_endless_line_is_a_transient_error_at_the_cap() {
    use tautly_provider::ProviderError;
    let mib = Bytes::from(vec![b'a'; 1 << 20]);
    let body = futures::stream::iter(std::iter::repeat_n(mib, 20));
    let out: Vec<_> = events(body).collect().await;
    assert_eq!(out.len(), 1, "{:?}", out.len());
    assert!(
        matches!(&out[0], Err(ProviderError::Transient { msg, .. }) if msg.contains("16 MiB")),
        "{:?}",
        out[0].as_ref().err()
    );
    // Data lines of one event count too.
    let line = Bytes::from(format!("data: {}\n", "b".repeat(1 << 20)));
    let body = futures::stream::iter(std::iter::repeat_n(line, 20));
    let out: Vec<_> = events(body).collect().await;
    assert!(matches!(
        out.last(),
        Some(Err(ProviderError::Transient { .. }))
    ));
}

#[tokio::test]
async fn many_events_past_the_cap_in_total_are_fine() {
    let ev = Bytes::from(format!("data: {}\n\n", "c".repeat(1 << 20)));
    let body = futures::stream::iter(std::iter::repeat_n(ev, 20));
    let out: Vec<_> = events(body).collect().await;
    assert_eq!(out.len(), 20);
    assert!(
        out.iter()
            .all(|e| e.as_ref().is_ok_and(|e| e.data.len() == 1 << 20))
    );
}

#[tokio::test]
async fn a_long_line_in_small_chunks_parses_once() {
    let line = format!("data: {}\n\n", "d".repeat(300_000));
    let chunks: Vec<&str> = line
        .as_bytes()
        .chunks(1000)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let out = parse(&chunks).await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].data.len(), 300_000);
}
