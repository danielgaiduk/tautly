//! Filesystem, process and network confinement for Tautly.

mod effect;
mod local;
mod path_search;
mod policy;
mod protected;
mod runners;
mod shell;
mod shell_state;
mod spill;
mod walk;

pub use effect::{Effect, FileMeta, HttpResponse, Sandbox, SandboxError, SpawnSpec};
pub use local::LocalSandbox;
pub use path_search::{which_outside, which_outside_or_skipped};
pub use policy::{Decision, Mode, Policy, PolicyConfig, PolicyError};
pub use protected::is_protected;
pub use shell::{Redirect, SimpleCommand, Unparseable, parse_commands};
pub use spill::{SPILL_DIR, SpillCleanup, cleanup_spills};
pub use tokio::process::Child;
pub use url::{Host, Url};
