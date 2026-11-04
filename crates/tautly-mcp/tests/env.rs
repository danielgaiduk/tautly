//! A stdio server's environment. This binary has one test because it changes the
//! process environment.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tautly_mcp::{McpConfig, McpHub, PROVIDER_KEY_VARS, ServerSource, ServerSpec};
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox};
use tautly_tools::ToolCx;

#[tokio::test]
async fn server_env_has_no_provider_keys() {
    // SAFETY: the only test in this binary, set before anything is spawned.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-parent");
        std::env::set_var("MY_GATEWAY_KEY", "sk-configured");
        std::env::set_var("TAUTLY_TEST_VISIBLE", "inherited");
    }
    let dir = tempfile::tempdir().unwrap();
    let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(dir.path().to_path_buf()).unwrap());
    let spec = ServerSpec::Stdio {
        command: common::fixture(),
        args: vec![],
        env: BTreeMap::from([
            ("ECHO_MARKER".to_string(), "from-config".to_string()),
            // Even set explicitly, a provider key variable is removed.
            ("OPENAI_API_KEY".to_string(), "sk-explicit".to_string()),
            // The engine's private pycache wins over the server's own (review S2-I1).
            (
                "PYTHONPYCACHEPREFIX".to_string(),
                "repo-pycache".to_string(),
            ),
        ]),
        cwd: None,
    };
    // What the engine passes: the default names plus every configured `api_key_env`.
    let mut strip: Vec<String> = PROVIDER_KEY_VARS.iter().map(|s| s.to_string()).collect();
    strip.push("MY_GATEWAY_KEY".into());
    let pycache = "/private/tautly-pycache".to_string();
    let hub = McpHub::start_with_env(
        vec![("echo".into(), spec, ServerSource::User)],
        Arc::clone(&sb),
        &McpConfig::default(),
        &strip,
        &[("PYTHONPYCACHEPREFIX".to_string(), pycache.clone())],
    )
    .await;
    assert!(hub.wait_started(Duration::from_secs(20)).await);
    let env = hub
        .ready_tools()
        .into_iter()
        .find(|t| t.name() == "mcp__echo__env")
        .unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".tautly/tmp/spill"));
    let r = env.run(json!({}), sb.as_ref(), &cx).await;
    assert!(!r.is_error, "{r:?}");
    let names: Vec<&str> = r
        .output
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    for hidden in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "MY_GATEWAY_KEY"] {
        assert!(!names.contains(&hidden), "{hidden} visible: {}", r.output);
    }
    assert!(!r.output.contains("sk-"), "{}", r.output);
    assert!(r.output.contains("ECHO_MARKER=from-config"));
    assert!(r.output.contains("TAUTLY_TEST_VISIBLE=inherited"));
    assert!(
        r.output
            .lines()
            .any(|l| l == format!("PYTHONPYCACHEPREFIX={pycache}")),
        "{}",
        r.output
    );
    hub.shutdown().await;
}
