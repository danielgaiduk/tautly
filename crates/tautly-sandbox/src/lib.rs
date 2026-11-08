//! Filesystem, process and network confinement for Tautly.
//!
//! - [`Sandbox`] is where tools touch the world: files confined to one root, processes and
//!   HTTP. [`LocalSandbox`] is the implementation on the local machine (no OS-level
//!   isolation: confinement is path resolution, protected paths and the policy).
//! - [`Policy`] decides whether a tool call runs, asks or is denied, from the permission
//!   [`Mode`] and the `tool(glob)` allow and deny rules ([`PolicyConfig`]). Shell commands
//!   are parsed with tree-sitter-bash ([`parse_commands`]) so rules see every simple
//!   command, its working directories and its write redirects; a command that cannot be
//!   analyzed always asks.
//! - [`is_protected`] names the paths that always ask (`.git/`, `.tautly/`, `.github/
//!   workflows/`, build and shell startup files).

mod deadline;
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

pub use effect::{Effect, FileMeta, HttpResponse, Sandbox, SandboxError, SpawnSpec, SpillWriter};
pub use local::LocalSandbox;
pub use path_search::{which_outside, which_outside_or_skipped};
pub use policy::{Decision, Mode, Policy, PolicyConfig, PolicyError};
pub use protected::{is_protected, lexical};
pub use shell::{Redirect, SimpleCommand, Unparseable, parse_commands};
pub use spill::{SPILL_DIR, SpillCleanup, cleanup_spills};
pub use tokio::process::Child;
pub use url::{Host, Url};

/// The builder every Tautly HTTP client starts from: rustls on the `ring` provider with the
/// webpki roots and HTTP/1.1 only (reqwest is built without a crypto provider of its own).
pub fn http_client_builder() -> reqwest::ClientBuilder {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
