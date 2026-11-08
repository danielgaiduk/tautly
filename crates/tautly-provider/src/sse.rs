//! Server-sent-events parser shared by the streaming adapters.

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::ProviderError;

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` lines, joined with newlines.
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
                    if parser.pending() > MAX_PENDING {
                        yield Err(ProviderError::Transient {
                            status: None,
                            retry_after: None,
                            msg: "stream error: an event grew past 16 MiB without ending".into(),
                        });
                        return;
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

/// Most bytes an unfinished event (its partial line plus its data so far) may hold.
const MAX_PENDING: usize = 16 << 20;

#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    /// Bytes of `buf` already consumed as lines (dropped when more input is needed).
    start: usize,
    /// Bytes of `buf` already searched for a line end, so a long line is scanned once.
    scanned: usize,
    event: Option<String>,
    data: Vec<String>,
    data_len: usize,
}

impl Parser {
    /// Bytes held for the event not yet dispatched.
    fn pending(&self) -> usize {
        self.buf.len() - self.start + self.data_len
    }

    /// Drop the consumed lines before waiting for more input.
    fn need_more(&mut self) -> Option<SseEvent> {
        self.buf.drain(..self.start);
        self.scanned -= self.start;
        self.start = 0;
        None
    }

    /// Consume complete lines until an event is dispatched. With `eof`, a trailing partial line
    /// and any pending event are flushed.
    fn next_event(&mut self, eof: bool) -> Option<SseEvent> {
        loop {
            let found = self.buf[self.scanned..]
                .iter()
                .position(|&b| b == b'\n' || b == b'\r');
            let Some(pos) = found.map(|off| self.scanned + off) else {
                self.scanned = self.buf.len();
                if !eof {
                    return self.need_more();
                }
                if self.start < self.buf.len() {
                    let line = self.buf.split_off(self.start);
                    self.field(&line);
                }
                self.buf.clear();
                (self.start, self.scanned) = (0, 0);
                return self.dispatch();
            };
            // A trailing CR may be the first half of CRLF; wait for the next chunk.
            if self.buf[pos] == b'\r' && pos + 1 == self.buf.len() && !eof {
                self.scanned = pos;
                return self.need_more();
            }
            let skip = if self.buf[pos] == b'\r' && self.buf.get(pos + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            let line = self.buf[self.start..pos].to_vec();
            self.start = pos + skip;
            self.scanned = self.start;
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
            "data" => {
                self.data_len += value.len();
                self.data.push(value.to_string());
            }
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        self.data_len = 0;
        if self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event,
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}
