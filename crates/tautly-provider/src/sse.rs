//! Server-sent-events parser shared by the streaming adapters.

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::ProviderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Parse an infallible byte stream into SSE events.
pub fn events(
    body: impl Stream<Item = Bytes>,
) -> impl Stream<Item = Result<SseEvent, ProviderError>> {
    try_events(body.map(Ok::<_, std::convert::Infallible>))
}

/// Parse a fallible byte stream (e.g. a `reqwest` body) into SSE events. A transport error is
/// yielded as a transient [`ProviderError`] and ends the stream.
pub fn try_events<E: std::fmt::Display>(
    body: impl Stream<Item = Result<Bytes, E>>,
) -> impl Stream<Item = Result<SseEvent, ProviderError>> {
    async_stream::stream! {
        let mut body = std::pin::pin!(body);
        let mut parser = Parser::default();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    parser.buf.extend_from_slice(&bytes);
                    while let Some(ev) = parser.next_event(false) {
                        yield Ok(ev);
                    }
                }
                Err(e) => {
                    yield Err(ProviderError::Transient { status: None, retry_after: None, msg: format!("stream error: {e}") });
                    return;
                }
            }
        }
        while let Some(ev) = parser.next_event(true) {
            yield Ok(ev);
        }
    }
}

#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl Parser {
    /// Consume complete lines until an event is dispatched. With `eof`, a trailing partial line
    /// and any pending event are flushed.
    fn next_event(&mut self, eof: bool) -> Option<SseEvent> {
        loop {
            let Some(pos) = self.buf.iter().position(|&b| b == b'\n' || b == b'\r') else {
                if !eof {
                    return None;
                }
                if !self.buf.is_empty() {
                    let line = std::mem::take(&mut self.buf);
                    self.field(&line);
                }
                return self.dispatch();
            };
            // A trailing CR may be the first half of CRLF; wait for the next chunk.
            if self.buf[pos] == b'\r' && pos + 1 == self.buf.len() && !eof {
                return None;
            }
            let skip = if self.buf[pos] == b'\r' && self.buf.get(pos + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            let line: Vec<u8> = self.buf.drain(..pos + skip).take(pos).collect();
            if line.is_empty() {
                if let Some(ev) = self.dispatch() {
                    return Some(ev);
                }
            } else {
                self.field(&line);
            }
        }
    }

    fn field(&mut self, line: &[u8]) {
        if line[0] == b':' {
            return;
        }
        let line = String::from_utf8_lossy(line);
        let (name, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match name {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event,
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}
