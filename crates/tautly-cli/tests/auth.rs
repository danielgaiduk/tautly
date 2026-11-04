//! `tautly auth add <provider>` (spec 7.11.2) against a file standing in for the keychain.
//! Feature `mock`: `cargo test -p tautly-cli --features mock`.

mod common;

use common::{Scratch, stderr, stdout};
use serde_json::json;

#[test]
fn auth_add_reads_stdin_and_writes_store() {
    let s = Scratch::new();
    let out = s.run_with_stdin(
        &mut s.command(&["auth", "add", "anthropic", "--stdin"]),
        "  sk-ant-test-123  \n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        "stored key for anthropic in the test secret store"
    );
    assert_eq!(
        s.secrets(),
        json!({"tautly": {"anthropic": "sk-ant-test-123"}})
    );

    // OpenRouter's account is the compat entry's; compat endpoints use their name.
    for (provider, account) in [
        ("openrouter", "compat:openrouter"),
        ("compat:local", "compat:local"),
        ("google", "google"),
    ] {
        let out = s.run_with_stdin(
            &mut s.command(&["auth", "add", provider, "--stdin"]),
            "key-2\n",
        );
        assert_eq!(out.status.code(), Some(0), "{provider}: {}", stderr(&out));
        assert!(stdout(&out).contains(&format!("stored key for {provider} in")));
        assert_eq!(s.secrets()["tautly"][account], "key-2", "{provider}");
    }
    // Only the first line counts; a key replaces the one stored before.
    let out = s.run_with_stdin(
        &mut s.command(&["auth", "add", "anthropic", "--stdin"]),
        "newer\nignored\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(s.secrets()["tautly"]["anthropic"], "newer");
}

#[test]
fn auth_add_rejects_blank() {
    let s = Scratch::new();
    for input in ["   \n", "", "\n"] {
        let out = s.run_with_stdin(&mut s.command(&["auth", "add", "openai", "--stdin"]), input);
        assert_eq!(out.status.code(), Some(1), "{input:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("blank"), "{}", stderr(&out));
        assert!(!s.store_path().exists(), "nothing stored for {input:?}");
    }
}

#[test]
fn auth_add_never_prints_key() {
    let s = Scratch::new();
    let key = "sk-secret-NEVER-PRINT-4242";
    let out = s.run_with_stdin(
        &mut s.command(&["auth", "add", "anthropic", "--stdin"]),
        &format!("{key}\n"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stdout(&out).contains(key) && !stderr(&out).contains(key));
    assert!(!stdout(&out).contains("NEVER"), "not even part of it");

    // Nor when storing fails.
    let mut cmd = s.command(&["auth", "add", "anthropic", "--stdin"]);
    cmd.env(
        "TAUTLY_TEST_SECRET_STORE",
        s.home.path().join("missing").join("secrets.json"),
    );
    let out = s.run_with_stdin(&mut cmd, &format!("{key}\n"));
    assert_eq!(out.status.code(), Some(1));
    assert!(!stdout(&out).contains(key) && !stderr(&out).contains(key));
}

#[test]
fn auth_add_unknown_provider_errors() {
    let s = Scratch::new();
    for provider in ["acme", "compat:", "compat:bad/name", "Anthropic", ""] {
        let out = s.run_with_stdin(&mut s.command(&["auth", "add", provider, "--stdin"]), "k\n");
        assert_eq!(out.status.code(), Some(2), "{provider:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains("anthropic") && err.contains("compat:<name>"),
            "{provider:?}: {err}"
        );
        assert!(!s.store_path().exists(), "nothing stored for {provider:?}");
    }
}

#[test]
fn auth_add_refuses_piped_stdin_without_the_flag() {
    let s = Scratch::new();
    let out = s.run_with_stdin(&mut s.command(&["auth", "add", "anthropic"]), "k\n");
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("--stdin"), "{}", stderr(&out));
    assert!(!s.store_path().exists());
}

#[test]
fn auth_add_without_a_keychain_points_at_the_env_variable() {
    let s = Scratch::new();
    for (provider, var) in [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("compat:local", "api_key_env"),
    ] {
        let mut cmd = s.command(&["auth", "add", provider, "--stdin"]);
        cmd.env(
            "TAUTLY_TEST_SECRET_STORE",
            s.home.path().join("missing").join("secrets.json"),
        );
        let out = s.run_with_stdin(&mut cmd, "k\n");
        assert_eq!(out.status.code(), Some(1), "{provider}");
        let err = stderr(&out);
        assert!(err.contains("no keychain"), "{provider}: {err}");
        assert!(err.contains(var), "{provider}: {err}");
    }
}
