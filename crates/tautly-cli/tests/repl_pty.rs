//! `tautly` on a terminal runs the REPL (spec 7.11.8): the real binary, with a scripted
//! provider (`--mock-script`), driven through a pseudo-terminal that answers the line
//! editor's cursor-position queries.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(60);

/// A `tautly` running on a pseudo-terminal, its output collected as it comes.
struct Pty {
    child: Child,
    master: File,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Pty {
    fn spawn(work: &Path, home: &Path, args: &[&str]) -> Pty {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two descriptors into the pointers it is given; the
        // optional name, termios and window size are null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: both descriptors were just opened by openpty and are owned here alone.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        let child = Command::new(env!("CARGO_BIN_EXE_tautly"))
            .args(args)
            .current_dir(work)
            .env("TAUTLY_HOME", home)
            .env("HOME", home)
            .env("NO_COLOR", "1")
            .env("TERM", "xterm")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdin(io(&slave))
            .stdout(io(&slave))
            .stderr(io(&slave))
            .spawn()
            .unwrap();
        drop(slave);
        let master = File::from(master);
        let out = Arc::new(Mutex::new(Vec::new()));
        let (mut reader, mut answer) = (master.try_clone().unwrap(), master.try_clone().unwrap());
        let sink = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let chunk = &buf[..n];
                for _ in 0..chunk.windows(4).filter(|w| *w == b"\x1b[6n").count() {
                    let _ = answer.write_all(b"\x1b[1;1R");
                }
                sink.lock().unwrap().extend_from_slice(chunk);
            }
        });
        Pty { child, master, out }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, what: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(what) {
            assert!(
                Instant::now() < deadline,
                "never saw {what:?} in:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
    }

    /// Ctrl+D at the prompt, then the exit code.
    fn finish(mut self) -> i32 {
        self.send("\x04");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "did not exit:\n{}", self.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn script(dir: &Path, text: &str) -> String {
    let path = dir.join(format!("{}.json", text.len()));
    let turns = json!([[{"text_delta": text}, {"done": {"stop": "end_turn"}}]]);
    std::fs::write(&path, turns.to_string()).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn repl_runs_on_a_terminal_and_continue_resumes_the_last_session() {
    let (work, home) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let first = script(home.path(), "Hello from the mock");
    let mut pty = Pty::spawn(
        work.path(),
        home.path(),
        &["--mock-script", &first, "--model", "mock/test"],
    );
    pty.wait_for("Ctrl+D to exit");
    pty.send("say hi\r");
    pty.wait_for("Hello from the mock");
    pty.wait_for("turn 1 ·");
    assert_eq!(pty.finish(), 0);
    let history = std::fs::read_to_string(home.path().join("repl_history")).unwrap();
    assert_eq!(history, "say hi\n");

    // `--continue` builds the first session as a resume: its earlier turn counts.
    let second = script(home.path(), "Back again, mock");
    let mut pty = Pty::spawn(
        work.path(),
        home.path(),
        &[
            "--mock-script",
            &second,
            "--model",
            "mock/test",
            "--continue",
        ],
    );
    pty.wait_for("Ctrl+D to exit");
    pty.send("/cost\r");
    pty.wait_for("session: 1 turns");
    pty.send("again\r");
    pty.wait_for("Back again, mock");
    pty.wait_for("turn 2 ·");
    assert_eq!(pty.finish(), 0);
}
