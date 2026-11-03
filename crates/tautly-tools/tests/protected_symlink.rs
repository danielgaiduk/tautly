//! Checkpoint fix C1: no write tool may reach a protected path through a symlink.

use std::path::Path;

use serde_json::json;
use tautly_provider::EditFormat;
use tautly_sandbox::{LocalSandbox, Sandbox};
use tautly_tools::{EditTool, ReadTool, Tool, ToolCx, WriteTool};
use tempfile::TempDir;

const CONFIG: &str = "[core]\n\tbare = false\n";

fn repo(format: EditFormat) -> (TempDir, LocalSandbox, ToolCx) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // Setup only; the tools under test go through the sandbox.
    #[allow(clippy::disallowed_methods)]
    {
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/config"), CONFIG).unwrap();
        std::fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    }
    std::os::unix::fs::symlink(".git/config", root.join("NOTES.md")).unwrap();
    std::os::unix::fs::symlink(".git/hooks/pre-commit", root.join("hook.sh")).unwrap();
    let sb = LocalSandbox::new(root.to_path_buf()).unwrap();
    let cx = ToolCx::new(format, root.join(".spill")).with_turn(1);
    (dir, sb, cx)
}

async fn config(sb: &LocalSandbox) -> String {
    String::from_utf8(sb.read(Path::new(".git/config")).await.unwrap()).unwrap()
}

async fn read(sb: &LocalSandbox, cx: &ToolCx, path: &str) -> String {
    let r = ReadTool.run(json!({"path": path}), sb, cx).await;
    assert!(!r.is_error, "{}", r.output);
    r.output
}

#[tokio::test]
async fn write_tool_refuses_symlink_to_protected_path() {
    let (_d, sb, cx) = repo(EditFormat::StrReplace);
    let r = WriteTool
        .run(json!({"path": "NOTES.md", "content": "pwned"}), &sb, &cx)
        .await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(config(&sb).await, CONFIG);
}

#[tokio::test]
async fn str_replace_refuses_symlink_to_protected_path() {
    let (_d, sb, cx) = repo(EditFormat::StrReplace);
    read(&sb, &cx, "NOTES.md").await;
    let r = EditTool
        .run(
            json!({"path": "NOTES.md", "old_text": "bare = false", "new_text": "fsmonitor = x"}),
            &sb,
            &cx,
        )
        .await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(config(&sb).await, CONFIG);
}

#[tokio::test]
async fn hashline_refuses_symlink_to_protected_path() {
    let (_d, sb, cx) = repo(EditFormat::Hashline);
    let out = read(&sb, &cx, "NOTES.md").await;
    let anchor = out
        .lines()
        .find(|l| l.starts_with("2:"))
        .and_then(|l| l.split('|').next())
        .unwrap()
        .to_string();
    let r = EditTool
        .run(
            json!({"path": "NOTES.md", "ops": [
                {"op": "replace", "start": anchor, "end": anchor, "lines": ["\tfsmonitor = x"]}
            ]}),
            &sb,
            &cx,
        )
        .await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(config(&sb).await, CONFIG);
}

#[tokio::test]
async fn patch_refuses_symlink_to_protected_path() {
    let (_d, sb, cx) = repo(EditFormat::Patch);
    read(&sb, &cx, "NOTES.md").await;
    read(&sb, &cx, "hook.sh").await;
    let patch = "*** Begin Patch\n*** Update File: NOTES.md\n [core]\n-\tbare = false\n+\tfsmonitor = x\n*** End Patch\n";
    let r = EditTool.run(json!({"patch": patch}), &sb, &cx).await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(config(&sb).await, CONFIG);
    let patch = "*** Begin Patch\n*** Delete File: hook.sh\n*** End Patch\n";
    let r = EditTool.run(json!({"patch": patch}), &sb, &cx).await;
    assert!(r.is_error, "{}", r.output);
    assert!(sb.read(Path::new(".git/hooks/pre-commit")).await.is_ok());
}
