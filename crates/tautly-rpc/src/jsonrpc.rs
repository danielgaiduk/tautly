//! JSON-RPC 2.0 framing over newline-delimited JSON: reading bounded lines, telling
//! requests from notifications, and writing responses, errors and notifications (one line
//! each). Batches are not supported.

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The longest line read; a longer one is skipped and answered with an error.
pub const MAX_LINE: usize = 8 * 1024 * 1024;

/// Invalid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// Valid JSON that is not a request.
pub const INVALID_REQUEST: i64 = -32600;
/// No such method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The params do not fit the method.
pub const INVALID_PARAMS: i64 = -32602;
/// A session could not be created, or a command could not be handed over.
pub const SESSION_FAILED: i64 = -32000;
/// No open session has the `session_id` given (or none was given and there is no default).
pub const UNKNOWN_SESSION: i64 = -32001;
/// A method other than `initialize` before a successful `initialize`.
pub const NOT_INITIALIZED: i64 = -32002;
/// `initialize` asked for a protocol major version this server does not speak.
pub const UNSUPPORTED_PROTOCOL: i64 = -32003;

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    /// One of the error code constants of this module.
    pub code: i64,
    /// A one-line description.
    pub message: String,
    /// More detail, when there is any.
    pub data: Option<Value>,
}

impl Error {
    /// An error without `data`.
    pub fn new(code: i64, message: impl Into<String>) -> Error {
        Error {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// The same error with `data`.
    pub fn with_data(mut self, data: Value) -> Error {
        self.data = Some(data);
        self
    }

    fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("code".into(), self.code.into());
        obj.insert("message".into(), self.message.clone().into());
        if let Some(data) = &self.data {
            obj.insert("data".into(), data.clone());
        }
        Value::Object(obj)
    }
}

/// A message from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Answered with a response carrying `id`.
    Request {
        /// The request id, echoed in the response.
        id: Value,
        /// The method name.
        method: String,
        /// The params (`null` when absent).
        params: Value,
    },
    /// No `id`: never answered, not even with an error.
    Notification {
        /// The method name.
        method: String,
        /// The params (`null` when absent).
        params: Value,
    },
}

impl Incoming {
    /// The method name.
    pub fn method(&self) -> &str {
        match self {
            Incoming::Request { method, .. } | Incoming::Notification { method, .. } => method,
        }
    }

    /// The params (`null` when absent).
    pub fn params(&self) -> &Value {
        match self {
            Incoming::Request { params, .. } | Incoming::Notification { params, .. } => params,
        }
    }

    /// The id to answer with; None for a notification.
    pub fn id(&self) -> Option<&Value> {
        match self {
            Incoming::Request { id, .. } => Some(id),
            Incoming::Notification { .. } => None,
        }
    }
}

/// Parse one line. On failure, the id to answer with (null when none could be read) and
/// the error; no id at all for a malformed *notification* (an object with a string
/// `method` and no `id`), which JSON-RPC never answers.
pub fn parse(line: &[u8]) -> Result<Incoming, (Option<Value>, Error)> {
    parse_value(parse_json(line)?)
}

/// The JSON of one line; a parse error is answered with id null.
pub fn parse_json(line: &[u8]) -> Result<Value, (Option<Value>, Error)> {
    serde_json::from_slice(line).map_err(|e| {
        (
            Some(Value::Null),
            Error::new(PARSE_ERROR, format!("parse error: {e}")),
        )
    })
}

/// [`parse`] for a line already read as JSON (a peer that also sends responses looks at
/// the value first).
pub fn parse_value(value: Value) -> Result<Incoming, (Option<Value>, Error)> {
    let Value::Object(mut obj) = value else {
        let what = if value.is_array() {
            "batches are not supported"
        } else {
            "a request must be a JSON object"
        };
        return Err((Some(Value::Null), Error::new(INVALID_REQUEST, what)));
    };
    let id = match obj.remove("id") {
        None => None,
        Some(id @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(id),
        Some(_) => {
            return Err((
                Some(Value::Null),
                Error::new(INVALID_REQUEST, "`id` must be a string, a number or null"),
            ));
        }
    };
    let method = match obj.remove("method") {
        Some(Value::String(method)) => method,
        _ => {
            // Without a method it is no notification, so it is answered.
            let id = id.unwrap_or(Value::Null);
            return Err((
                Some(id),
                Error::new(INVALID_REQUEST, "`method` must be a string"),
            ));
        }
    };
    // From here on, a message without an id is a notification: never answered.
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((id, Error::new(INVALID_REQUEST, "`jsonrpc` must be \"2.0\"")));
    }
    let params = match obj.remove("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p,
        Some(_) => {
            return Err((
                id,
                Error::new(INVALID_PARAMS, "`params` must be an object (by name)"),
            ));
        }
    };
    Ok(match id {
        Some(id) => Incoming::Request { id, method, params },
        None => Incoming::Notification { method, params },
    })
}

/// A success response line (without the newline).
pub fn response(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// An error response line (without the newline).
pub fn error_response(id: &Value, err: &Error) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": err.to_value()}).to_string()
}

/// A notification line (without the newline).
pub fn notification(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// One read from the input.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A line, without its newline (the last one may have none).
    Line(Vec<u8>),
    /// A line longer than the limit; it was read to its end and dropped.
    TooLong,
    /// The input ended.
    Eof,
}

/// Reads lines, keeping at most `max` bytes of each in memory. Unlike [`read_line`], a
/// call that is dropped before it finishes (a `select!` branch losing) loses nothing: what
/// it had read stays here for the next call.
#[derive(Default)]
pub struct LineReader {
    line: Vec<u8>,
    too_long: bool,
}

impl LineReader {
    /// The next line. Cancel safe.
    pub async fn read(
        &mut self,
        input: &mut (impl AsyncBufRead + Unpin),
        max: usize,
    ) -> std::io::Result<Line> {
        loop {
            let buf = input.fill_buf().await?;
            if buf.is_empty() {
                let (too_long, line) = (self.too_long, std::mem::take(&mut self.line));
                self.too_long = false;
                return Ok(match (too_long, line.is_empty()) {
                    (true, _) => Line::TooLong,
                    (false, true) => Line::Eof,
                    (false, false) => Line::Line(line),
                });
            }
            let (chunk, used, end) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..i], i + 1, true),
                None => (buf, buf.len(), false),
            };
            if !self.too_long {
                if self.line.len() + chunk.len() > max {
                    self.too_long = true;
                    self.line = Vec::new();
                } else {
                    self.line.extend_from_slice(chunk);
                }
            }
            input.consume(used);
            if end {
                let line = std::mem::take(&mut self.line);
                return Ok(if std::mem::take(&mut self.too_long) {
                    Line::TooLong
                } else {
                    Line::Line(line)
                });
            }
        }
    }
}

/// Read the next line, keeping at most `max` bytes of it in memory. Not cancel safe: use
/// a [`LineReader`] where the call may be dropped.
pub async fn read_line(
    input: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> std::io::Result<Line> {
    LineReader::default().read(input, max).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_notifications_and_bad_messages() {
        let req = parse(br#"{"jsonrpc":"2.0","id":7,"method":"cancel"}"#).unwrap();
        assert_eq!(req.id(), Some(&json!(7)));
        assert_eq!(req.params(), &json!({}));
        let note = parse(br#"{"jsonrpc":"2.0","method":"cancel","params":{"a":1}}"#).unwrap();
        assert_eq!(note.id(), None);
        assert_eq!(note.params(), &json!({"a": 1}));

        let code = |line: &[u8]| parse(line).unwrap_err();
        assert_eq!(code(b"{nope"), (Some(Value::Null), code(b"{nope").1));
        assert_eq!(code(b"{nope").1.code, PARSE_ERROR);
        assert_eq!(code(b"[1]").1.code, INVALID_REQUEST);
        let (id, err) = code(br#"{"jsonrpc":"1.0","id":"x","method":"m"}"#);
        assert_eq!((id, err.code), (Some(json!("x")), INVALID_REQUEST));
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","id":1,"method":"m","params":[1]}"#),
            (
                Some(json!(1)),
                Error::new(INVALID_PARAMS, "`params` must be an object (by name)")
            )
        );
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","id":{},"method":"m"}"#).1.code,
            INVALID_REQUEST
        );
        // Not even a malformed notification is answered.
        for note in [
            &br#"{"jsonrpc":"2.0","method":"m","params":[1]}"#[..],
            br#"{"jsonrpc":"1.0","method":"m"}"#,
            br#"{"method":"m","params":"x"}"#,
        ] {
            let (id, err) = code(note);
            assert_eq!(id, None, "{}: {err:?}", String::from_utf8_lossy(note));
        }
        // Without a string `method` it is no notification: answered with id null.
        assert_eq!(
            code(br#"{"jsonrpc":"2.0","method":1}"#).0,
            Some(Value::Null)
        );
        assert_eq!(code(br#"{"jsonrpc":"2.0"}"#).0, Some(Value::Null));
    }

    #[tokio::test]
    async fn long_lines_are_dropped_and_reading_goes_on() {
        let data = format!("{}\nok\nlast", "x".repeat(100));
        let mut input = tokio::io::BufReader::with_capacity(7, data.as_bytes());
        assert_eq!(read_line(&mut input, 10).await.unwrap(), Line::TooLong);
        assert_eq!(
            read_line(&mut input, 10).await.unwrap(),
            Line::Line(b"ok".to_vec())
        );
        assert_eq!(
            read_line(&mut input, 10).await.unwrap(),
            Line::Line(b"last".to_vec())
        );
        assert_eq!(read_line(&mut input, 10).await.unwrap(), Line::Eof);
    }

    #[tokio::test]
    async fn a_dropped_read_loses_nothing() {
        use tokio::io::AsyncWriteExt;
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut input = tokio::io::BufReader::new(rx);
        let mut reader = LineReader::default();
        tx.write_all(b"ab").await.unwrap();
        // The read gives up half way through the line (another `select!` branch won).
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reader.read(&mut input, 10),
        )
        .await;
        assert!(waited.is_err());
        tx.write_all(b"cd\nef\n").await.unwrap();
        assert_eq!(
            reader.read(&mut input, 10).await.unwrap(),
            Line::Line(b"abcd".to_vec())
        );
        assert_eq!(
            reader.read(&mut input, 10).await.unwrap(),
            Line::Line(b"ef".to_vec())
        );
    }
}
