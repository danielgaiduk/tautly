//! Builds `echo_server` with the workspace tests (an integration test makes Cargo build
//! the package's binaries), and checks it answers the legacy handshake.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn legacy_mode_answers_initialize() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_echo_server"))
        .arg("legacy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(stdin);
    let _ = child.wait();
    assert!(line.contains("2025-06-18"), "{line}");
}
