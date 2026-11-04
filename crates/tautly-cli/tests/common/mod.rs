//! A scratch home and work tree for running the `tautly` binary. The keychain is a file
//! (`TAUTLY_TEST_SECRET_STORE`), Ollama is a closed loopback port
//! (`TAUTLY_TEST_OLLAMA_PORT`) and no provider key variable is inherited, so a run never
//! reads the real keychain, a real Ollama or the developer's keys.

#![allow(dead_code)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

pub const KEY_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
];

pub struct Scratch {
    pub home: TempDir,
    pub work: TempDir,
    pub ollama_port: u16,
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            ollama_port: closed_port(),
        }
    }

    /// The file standing in for the keychain (absent until something is stored).
    pub fn store_path(&self) -> PathBuf {
        self.home.path().join("secrets.json")
    }

    /// What the keychain file holds (`{}` when nothing was stored).
    pub fn secrets(&self) -> Value {
        match std::fs::read_to_string(self.store_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap(),
            Err(_) => Value::Object(Default::default()),
        }
    }

    /// Put `secret` in the keychain file under service `tautly`.
    pub fn store_secret(&self, account: &str, secret: &str) {
        let mut all = self.secrets();
        all.as_object_mut()
            .unwrap()
            .entry("tautly")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .unwrap()
            .insert(account.into(), Value::String(secret.into()));
        std::fs::write(self.store_path(), all.to_string()).unwrap();
    }

    /// Write the user configuration (`TAUTLY_HOME/config.toml`).
    pub fn user_config(&self, toml: &str) {
        std::fs::write(self.home.path().join("config.toml"), toml).unwrap();
    }

    /// Write the project configuration (`<work>/.tautly/config.toml`).
    pub fn project_config(&self, toml: &str) {
        let dir = self.work.path().join(".tautly");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    /// Write a mock script of `turns` and return its path.
    pub fn script(&self, turns: Value) -> String {
        let path = self.home.path().join("script.json");
        std::fs::write(&path, turns.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_tautly"));
        c.args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env("TAUTLY_TEST_SECRET_STORE", self.store_path())
            .env("TAUTLY_TEST_OLLAMA_PORT", self.ollama_port.to_string())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        for var in KEY_VARS {
            c.env_remove(var);
        }
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run with `input` on stdin (a pipe, not a terminal).
    pub fn run_with_stdin(&self, command: &mut Command, input: &str) -> Output {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

/// A loopback port nothing listens on.
pub fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
