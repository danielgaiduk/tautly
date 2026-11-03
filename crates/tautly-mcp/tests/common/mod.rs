//! The `echo_server` fixture (crate `tautly-mcp-fixture`).

use std::sync::OnceLock;

/// The fixture binary next to this test's directory. `cargo test --workspace` builds it;
/// otherwise it is built here once.
pub fn fixture() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().unwrap().parent().unwrap().join("echo_server");
        if !bin.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-p", "tautly-mcp-fixture", "--bin", "echo_server"])
                .status()
                .unwrap();
            assert!(status.success(), "could not build the echo_server fixture");
        }
        assert!(bin.exists(), "no fixture at {}", bin.display());
        bin.to_string_lossy().into_owned()
    })
    .clone()
}
