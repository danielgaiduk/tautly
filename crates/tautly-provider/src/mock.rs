//! Deterministic scripted provider for tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::{CanonicalRequest, Provider, ProviderError, ProviderEvent, RawUsage, StopReason};

/// One scripted response.
#[derive(Debug, Clone)]
pub enum MockTurn {
    /// Yields these events.
    Events(Vec<ProviderEvent>),
    /// Fails with this error.
    Error(ProviderError),
    /// Yields the events, then a `Transient` error.
    DropAfter(Vec<ProviderEvent>),
    /// Yields nothing until cancelled.
    Stall,
}

#[derive(Default)]
struct State {
    script: VecDeque<MockTurn>,
    requests: Vec<CanonicalRequest>,
}

/// Clones share the script and the request log.
#[derive(Clone)]
pub struct MockProvider {
    state: Arc<Mutex<State>>,
}

impl MockProvider {
    /// A provider answering successive requests from `script`.
    pub fn new(script: Vec<MockTurn>) -> Self {
        let state = State {
            script: script.into(),
            requests: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<CanonicalRequest> {
        self.state.lock().unwrap().requests.clone()
    }
}

type Item = Result<ProviderEvent, ProviderError>;

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream(&self, req: CanonicalRequest, cancel: CancellationToken) -> BoxStream<'static, Item> {
        let turn = {
            let mut st = self.state.lock().unwrap();
            st.requests.push(req);
            st.script.pop_front()
        };
        match turn {
            None => {
                stream::iter([Err(ProviderError::Other("mock script exhausted".into()))]).boxed()
            }
            Some(MockTurn::Events(evs)) => stream::iter(evs.into_iter().map(Ok)).boxed(),
            Some(MockTurn::Error(e)) => stream::iter([Err(e)]).boxed(),
            Some(MockTurn::DropAfter(evs)) => {
                let drop = ProviderError::Transient {
                    status: None,
                    retry_after: None,
                    msg: "mock connection dropped".into(),
                };
                stream::iter(evs.into_iter().map(Ok).chain([Err(drop)])).boxed()
            }
            Some(MockTurn::Stall) => stream::once(async move {
                cancel.cancelled().await;
            })
            .filter_map(|()| async { None })
            .boxed(),
        }
    }
}

/// A tool call as the model sends it.
pub fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

/// Usage of 100 input and 10 output tokens.
pub fn usage() -> ProviderEvent {
    ProviderEvent::Usage(RawUsage {
        input: 100,
        output: 10,
        cache_read: 0,
        cache_write: 0,
    })
}

/// A response with tool calls. It reports no usage: the mock's numbers would calibrate
/// the token estimator far off.
pub fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

/// A final text answer that reports [`usage`].
pub fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        usage(),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

/// A final text answer that reports no usage.
pub fn text_step_no_usage(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}
