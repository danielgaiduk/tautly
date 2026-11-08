//! Fixtures shared by the server and one-shot tests: a scripted provider, a temporary
//! work tree and data home, and an engine factory.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::Value;
use tautly_core::{
    CliOverrides, Config, Engine, EngineBuilder, Event, Mode, ModelId, Workspace, Workspaces,
};
pub use tautly_provider::mock::{call, text_step, tools_step};
use tautly_provider::{Catalog, MockProvider};
use tautly_rpc::{EngineFactory, SessionSpec};
use tautly_sandbox::{LocalSandbox, Sandbox};
use tokio::sync::mpsc;

pub use tautly_testkit::{Fx, MODEL, WAIT};

pub fn catalog() -> Catalog {
    tautly_testkit::catalog(200_000, 8000, "str_replace")
}

/// The configuration `tautly` loads for these CLI flags (no user or project file), on the
/// mock model.
pub fn config_with(cli: CliOverrides, root: &std::path::Path) -> Config {
    let cli = CliOverrides {
        model: Some(ModelId::from(MODEL)),
        ..cli
    };
    let mut loaded = Config::load_from(None, root, cli).unwrap();
    loaded.config.catalog_overrides = Some(catalog());
    loaded.config
}

pub fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog()),
        ..Config::default()
    };
    c.permissions.mode = mode;
    c
}

/// What the tests ask of [`Fx`] beyond the shared fixture.
#[allow(async_fn_in_trait)]
pub trait FxExt {
    fn exists(&self, rel: &str) -> bool;
    fn build(&self, b: EngineBuilder) -> (Engine, mpsc::Receiver<Event>);
    fn workspaces(&self) -> Workspaces;
    async fn ready_workspace(&self) -> Arc<Workspace>;
}

impl FxExt for Fx {
    fn exists(&self, rel: &str) -> bool {
        self.dir.path().join(rel).exists()
    }

    fn build(&self, b: EngineBuilder) -> (Engine, mpsc::Receiver<Event>) {
        b.build().unwrap()
    }

    fn workspaces(&self) -> Workspaces {
        Workspaces::new(self.home.path())
    }

    /// A workspace over this root whose index and memory store are open.
    async fn ready_workspace(&self) -> Arc<Workspace> {
        let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(self.root()).unwrap());
        let ws = Workspace::new(sb, self.home.path());
        ws.start();
        assert!(ws.wait_ready_or_failed().await, "index never became ready");
        let deadline = tokio::time::Instant::now() + WAIT;
        while ws.memory().is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "memory never opened"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ws
    }
}

/// What a factory made: each session's spec (root, resume, interactive) and workspace.
#[derive(Default)]
pub struct Made {
    pub specs: Vec<(PathBuf, Option<String>, bool)>,
    pub workspaces: Vec<Weak<Workspace>>,
    pub strong: Vec<Arc<Workspace>>,
}

/// A factory building each new session on the next of `mocks` (an empty script once they
/// run out), with `cfg`, recording what it was asked for. With `keep`, it also holds each
/// workspace (so a test can inspect it); otherwise only weakly.
pub fn factory(
    fx: &Fx,
    cfg: Config,
    mocks: Vec<MockProvider>,
    keep: bool,
) -> (EngineFactory, Arc<Mutex<Made>>) {
    let made = Arc::new(Mutex::new(Made::default()));
    let mocks = Arc::new(Mutex::new(VecDeque::from(mocks)));
    let home = fx.home.path().to_path_buf();
    let record = Arc::clone(&made);
    let f: EngineFactory = Arc::new(move |spec: SessionSpec| {
        let mock = mocks
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockProvider::new(vec![]));
        {
            let mut m = record.lock().unwrap();
            m.specs
                .push((spec.root.clone(), spec.resume.clone(), spec.interactive));
            m.workspaces.push(Arc::downgrade(&spec.workspace));
            if keep {
                m.strong.push(Arc::clone(&spec.workspace));
            }
        }
        let b = EngineBuilder {
            provider: Some(Arc::new(mock)),
            data_home: Some(home.clone()),
            workspace: Some(spec.workspace),
            interactive: spec.interactive,
            resume: spec.resume,
            ..EngineBuilder::new(cfg.clone(), spec.root)
        };
        Ok(b.build()?)
    });
    (f, made)
}

/// The JSON of an event, as on the wire.
pub fn wire(ev: &Event) -> Value {
    serde_json::to_value(ev).unwrap()
}
