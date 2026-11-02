//! Deterministic scripted provider for tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{CanonicalRequest, Provider, ProviderError, ProviderEvent};

#[derive(Debug, Clone)]
pub enum MockTurn {
    Events(Vec<ProviderEvent>),
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
    pub fn new(script: Vec<MockTurn>) -> Self {
        let state = State { script: script.into(), requests: Vec::new() };
        Self { state: Arc::new(Mutex::new(state)) }
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
            None => stream::iter([Err(ProviderError::Other("mock script exhausted".into()))]).boxed(),
            Some(MockTurn::Events(evs)) => stream::iter(evs.into_iter().map(Ok)).boxed(),
            Some(MockTurn::Error(e)) => stream::iter([Err(e)]).boxed(),
            Some(MockTurn::DropAfter(evs)) => {
                let drop = ProviderError::Transient { status: None, retry_after: None, msg: "mock connection dropped".into() };
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
