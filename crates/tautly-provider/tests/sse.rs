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
