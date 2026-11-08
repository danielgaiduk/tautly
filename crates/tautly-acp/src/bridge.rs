//! The ACP connection (spec 7.11.9): one JSON-RPC 2.0 message per line in each direction.
//!
//! The reader handles each message in order. Whatever must reach an engine in order (a
//! prompt's message, a cancel, a model switch) is sent before the next message is read;
//! only the waiting (for a turn's end, a session's build, a client's answer) runs in a
//! task. Each session has a forwarding task that turns its events into `session/update`
//! notifications and its approval requests into `session/request_permission` requests,
//! each awaited in a task of its own so events keep flowing.
//!
//! Ordering: `session/new` answers before any of its session's updates; `session/load`
//! replays the session's history before it answers. Every line goes through one writer.

mod conn;
mod events;
mod replay;

use conn::Conn;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tautly_core::printable;
use tautly_rpc::jsonrpc::{self, Error, INVALID_REQUEST, Line, MAX_LINE};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::{AcpEngineFactory, AcpError, ModelLister};

/// The answer to a request, once what it waits for has happened.
type Reply = Pin<Box<dyn Future<Output = Result<Value, Error>> + Send>>;

/// Internal error (JSON-RPC): a session that could not be opened, a turn that failed.
const INTERNAL_ERROR: i64 = -32603;
/// ACP `resource_not_found`: no open session has that id.
const NOT_FOUND: i64 = -32002;
/// Lines waiting for the output before producers wait.
const OUTPUT_CAPACITY: usize = 1024;
/// Updates of a loaded session read ahead of the client.
const REPLAY_BOUND: usize = 64;

/// An ACP agent serving sessions from a factory.
pub struct Bridge {
    factory: AcpEngineFactory,
    models: Option<ModelLister>,
    data_home: Option<PathBuf>,
    stop: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Bridge {
    pub fn new(factory: AcpEngineFactory) -> Bridge {
        Bridge {
            factory,
            models: None,
            data_home: None,
            stop: None,
        }
    }

    /// The data home whose cost ledger holds a loaded session's earlier spend, so its
    /// `usage_update` cost goes on from there (none: from 0).
    pub fn data_home(mut self, data_home: impl Into<PathBuf>) -> Bridge {
        self.data_home = Some(data_home.into());
        self
    }

    /// What the `model` config option offers (none: no config options).
    pub fn models(mut self, models: ModelLister) -> Bridge {
        self.models = Some(models);
        self
    }

    /// Stop serving when `stop` completes (SIGTERM, SIGHUP), as at the end of input.
    pub fn stop_on(mut self, stop: impl Future<Output = ()> + Send + 'static) -> Bridge {
        self.stop = Some(Box::pin(stop));
        self
    }

    /// Serve until the input ends or the stop future fires; every session is shut down
    /// and its last events written before this returns. An error only when the input
    /// cannot be read (the sessions are shut down all the same).
    pub async fn serve(
        self,
        input: impl AsyncRead + Unpin,
        output: impl AsyncWrite + Unpin,
    ) -> Result<(), AcpError> {
        let (out, lines) = mpsc::channel(OUTPUT_CAPACITY);
        let conn = Arc::new(Conn {
            out,
            factory: self.factory,
            models: self.models,
            data_home: self.data_home,
            next_id: AtomicU64::new(1),
            state: Mutex::default(),
        });
        let stop = self
            .stop
            .unwrap_or_else(|| Box::pin(std::future::pending()));
        let reader = read_loop(conn, input, stop);
        let (result, ()) = tokio::join!(reader, write_lines(lines, output));
        result
    }
}

async fn write_lines(mut lines: mpsc::Receiver<String>, mut output: impl AsyncWrite + Unpin) {
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
            // The client is gone; keep draining so producers never wait on it.
            while lines.recv().await.is_some() {}
            return;
        }
    }
}

/// Diagnostics: stderr only (stdout carries the protocol), escaped for the terminal.
fn log(msg: &str) {
    eprintln!("tautly: {}", printable(msg));
}

async fn read_loop(
    conn: Arc<Conn>,
    input: impl AsyncRead + Unpin,
    mut stop: Pin<Box<dyn Future<Output = ()> + Send>>,
) -> Result<(), AcpError> {
    let mut input = BufReader::new(input);
    let mut reader = jsonrpc::LineReader::default();
    let mut tasks = JoinSet::new();
    let mut result = Ok(());
    loop {
        while tasks.try_join_next().is_some() {}
        let read = tokio::select! {
            read = reader.read(&mut input, MAX_LINE) => read,
            () = &mut stop => break,
        };
        match read {
            Ok(Line::Eof) => break,
            Err(e) => {
                result = Err(AcpError::Input(e));
                break;
            }
            Ok(Line::TooLong) => {
                let err = Error::new(
                    INVALID_REQUEST,
                    format!("a message may be at most {MAX_LINE} bytes"),
                );
                conn.send(jsonrpc::error_response(&Value::Null, &err)).await;
            }
            Ok(Line::Line(line)) => {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let value = match jsonrpc::parse_json(&line) {
                    Ok(v) => v,
                    Err((id, err)) => {
                        let id = id.unwrap_or(Value::Null);
                        conn.send(jsonrpc::error_response(&id, &err)).await;
                        continue;
                    }
                };
                let Some(value) = conn.route_response(value) else {
                    continue;
                };
                match jsonrpc::parse_value(value) {
                    Err((Some(id), err)) => conn.send(jsonrpc::error_response(&id, &err)).await,
                    Err((None, _)) => {}
                    Ok(jsonrpc::Incoming::Notification { method, params }) => {
                        Arc::clone(&conn).notification(&method, params).await;
                    }
                    Ok(jsonrpc::Incoming::Request { id, method, params }) => {
                        Arc::clone(&conn)
                            .request(id, &method, params, &mut tasks)
                            .await;
                    }
                }
            }
        }
    }
    conn.close().await;
    tasks.shutdown().await;
    result
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
