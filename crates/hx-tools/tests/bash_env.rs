//! Checkpoint fix round 4: shell-state variables inherited by hx never reach the
//! bash child. Its own test binary with one test, because it sets the process
//! environment.

use hx_provider::EditFormat;
use hx_sandbox::LocalSandbox;
use hx_tools::{BashTool, Tool, ToolCx};
use serde_json::json;

#[test]
fn inherited_shell_state_variables_are_removed() {
    // SAFETY: this binary has exactly this one test and sets the variables before
    // starting the runtime or spawning anything, so no other thread reads the
    // environment concurrently.
    unsafe {
        std::env::set_var("CDPATH", ".git");
        std::env::set_var("BASH_ENV", "/nonexistent/hx-bash-env");
        std::env::set_var("ENV", "/nonexistent/hx-env");
        std::env::set_var("PROMPT_COMMAND", "true");
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    let r = rt.block_on(BashTool.run(
        json!({"command": "echo \"[$CDPATH|$BASH_ENV|$ENV|$PROMPT_COMMAND]\""}),
        &sb,
        &cx,
    ));
    assert_eq!(r.output, "[|||]\nexit code: 0");
}
