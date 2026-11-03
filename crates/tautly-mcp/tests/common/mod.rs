//! The `echo_server` fixture (crate `tautly-mcp-fixture`).

use std::sync::OnceLock;

/// The fixture binary next to this test's directory, brought up to date once per test
/// binary (`cargo test -p tautly-mcp` alone does not rebuild another package's binary, so
/// it could be stale).
pub fn fixture() -> String {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().unwrap().parent().unwrap().join("echo_server");
        {
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
