//! Where the REPL's lines come from: the terminal ([`ReedlineSource`]) or a script
//! ([`ScriptedSource`], tests); and the history file, which only the REPL writes.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use reedline::{
    Color, EditCommand, Emacs, FileBackedHistory, History, HistoryItem, KeyCode, KeyModifiers,
    Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent, Signal,
    default_emacs_keybindings,
};
use tokio::sync::mpsc;

use crate::ReplError;

/// Entries kept in the history file.
pub const HISTORY_ENTRIES: usize = 1000;

/// One answer from the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A line of text (without the newline).
    Text(String),
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+D (or the end of input).
    CtrlD,
}

/// Where the REPL reads its input: the terminal, or a script in tests.
#[async_trait]
pub trait LineSource: Send {
    /// A line typed at `prompt` (a message or a slash command).
    async fn read_line(&mut self, prompt: &str) -> Result<Line, ReplError>;

    /// An answer to a question (an approval, `/undo`'s confirmation): like
    /// [`read_line`](Self::read_line), but never recalled from history.
    async fn read_answer(&mut self, prompt: &str) -> Result<Line, ReplError> {
        self.read_line(prompt).await
    }
}

/// Lines from a script, for tests: a fixed list (then Ctrl+D), or lines sent on a channel
/// as the test goes (Ctrl+D once it closes). Every prompt shown is recorded.
pub struct ScriptedSource {
    lines: VecDeque<Line>,
    feed: Option<mpsc::UnboundedReceiver<Line>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedSource {
    /// A source answering with `lines`, then Ctrl+D.
    pub fn new(lines: Vec<Line>) -> Self {
        ScriptedSource {
            lines: lines.into(),
            feed: None,
            prompts: Arc::default(),
        }
    }

    /// A source answering with lines sent on the returned channel, then Ctrl+D once it closes.
    pub fn channel() -> (mpsc::UnboundedSender<Line>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        let source = ScriptedSource {
            feed: Some(rx),
            ..ScriptedSource::new(Vec::new())
        };
        (tx, source)
    }

    /// The prompts shown so far, shared with the source.
    pub fn prompts(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.prompts)
    }
}

#[async_trait]
impl LineSource for ScriptedSource {
    async fn read_line(&mut self, prompt: &str) -> Result<Line, ReplError> {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(prompt.to_string());
        if let Some(line) = self.lines.pop_front() {
            return Ok(line);
        }
        match &mut self.feed {
            Some(rx) => Ok(rx.recv().await.unwrap_or(Line::CtrlD)),
            None => Ok(Line::CtrlD),
        }
    }
}

/// The prompt reedline draws: our text, nothing else.
struct PlainPrompt {
    text: String,
    color: bool,
}

impl Prompt for PlainPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }
    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("(search) ")
    }
    fn get_prompt_color(&self) -> Color {
        if self.color {
            Color::Cyan
        } else {
            Color::Reset
        }
    }
    fn get_indicator_color(&self) -> Color {
        Color::Reset
    }
}

/// The terminal, through `reedline`: line editing, Alt+Enter and bracketed paste for
/// several lines, and history recall of earlier messages (from the history file, which
/// the REPL writes; answers to questions are read by a separate editor and never
/// recalled). Each read runs on the blocking pool.
pub struct ReedlineSource {
    editor: Option<Reedline>,
    answers: Option<Reedline>,
    color: bool,
    /// The editor has read a line: a later error is a real one, not a terminal it cannot drive.
    worked: bool,
    /// Plain line input: the terminal could not drive the editor (it never answered the
    /// cursor-position query, as some embedded terminals do).
    plain: bool,
}

impl ReedlineSource {
    /// An editor whose history starts with the entries of `history` (if it exists).
    pub fn new(history: Option<&Path>, color: bool) -> Result<Self, ReplError> {
        let mut recall = FileBackedHistory::new(HISTORY_ENTRIES)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(path) = history {
            for entry in read_history(path) {
                let _ = recall.save(HistoryItem::from_command_line(entry));
            }
        }
        let main = editor().with_history(Box::new(recall));
        let answers = editor().with_history(Box::new(
            FileBackedHistory::new(1).map_err(|e| std::io::Error::other(e.to_string()))?,
        ));
        Ok(ReedlineSource {
            editor: Some(main),
            answers: Some(answers),
            color,
            worked: false,
            plain: false,
        })
    }

    async fn read_with(&mut self, answer: bool, prompt: &str) -> Result<Line, ReplError> {
        if self.plain {
            return plain_line(prompt).await;
        }
        let slot = if answer {
            &mut self.answers
        } else {
            &mut self.editor
        };
        let mut editor = slot
            .take()
            .ok_or_else(|| std::io::Error::other("the line editor was lost"))?;
        let raw = prompt.to_string();
        let prompt = PlainPrompt {
            text: prompt.to_string(),
            color: self.color,
        };
        let (editor, signal) = tokio::task::spawn_blocking(move || {
            let signal = editor.read_line(&prompt);
            (editor, signal)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        *slot = Some(editor);
        let signal = match signal {
            Ok(s) => s,
            Err(e) if !self.worked => {
                self.plain = true;
                eprintln!(
                    "tautly: this terminal cannot run the line editor ({e}); using plain line \
                     input (no history recall or multi-line editing)"
                );
                return plain_line(&raw).await;
            }
            Err(e) => return Err(e.into()),
        };
        self.worked = true;
        Ok(match signal {
            Signal::Success(text) => Line::Text(text),
            Signal::CtrlC => Line::CtrlC,
            Signal::CtrlD => Line::CtrlD,
            _ => Line::Text(String::new()),
        })
    }
}

/// One line from stdin after printing `prompt`, on the blocking pool.
async fn plain_line(prompt: &str) -> Result<Line, ReplError> {
    let prompt = prompt.to_string();
    let line = tokio::task::spawn_blocking(move || {
        read_plain(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
            &prompt,
        )
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(line)
}

/// `prompt` to `out`, then one line of `input` without its line end; Ctrl+D at its end.
fn read_plain(
    input: &mut impl std::io::BufRead,
    out: &mut impl std::io::Write,
    prompt: &str,
) -> std::io::Result<Line> {
    out.write_all(prompt.as_bytes())?;
    out.flush()?;
    let mut text = String::new();
    if input.read_line(&mut text)? == 0 {
        return Ok(Line::CtrlD);
    }
    let end = text.trim_end_matches(['\n', '\r']).len();
    text.truncate(end);
    Ok(Line::Text(text))
}

fn editor() -> Reedline {
    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    Reedline::create()
        .with_edit_mode(Box::new(Emacs::new(keys)))
        .use_bracketed_paste(true)
}

#[async_trait]
impl LineSource for ReedlineSource {
    async fn read_line(&mut self, prompt: &str) -> Result<Line, ReplError> {
        self.read_with(false, prompt).await
    }

    async fn read_answer(&mut self, prompt: &str) -> Result<Line, ReplError> {
        self.read_with(true, prompt).await
    }
}

/// The history file: one entry per line, with `\` and newlines escaped (`\\`, `\n`), at
/// most [`HISTORY_ENTRIES`] of them (the oldest dropped), readable by its owner only.
pub(crate) struct HistoryFile {
    path: Option<PathBuf>,
}

impl HistoryFile {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        HistoryFile { path }
    }

    /// Append `entry`; a file that cannot be written is left as it is (history is a
    /// convenience, never a reason to stop). Concurrent REPLs take turns: the update holds
    /// an exclusive lock on `<file>.lock`, so none loses another's entry.
    pub(crate) fn add(&self, entry: &str) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(lock) = open_private(&lock_path(path), false) else {
            return;
        };
        if lock.lock().is_err() {
            return;
        }
        let mut entries = read_history(path);
        if entries.last().map(String::as_str) == Some(entry) {
            return;
        }
        entries.push(entry.to_string());
        let skip = entries.len().saturating_sub(HISTORY_ENTRIES);
        let text: String = entries[skip..]
            .iter()
            .map(|e| format!("{}\n", escape(e)))
            .collect();
        let _ = write_private(path, &text);
        // The lock is released when `lock` is dropped.
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Open (creating) a file readable by its owner only; `truncate` empties it.
fn open_private(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
}

/// Replace `path` with `text` through a temporary file of a name no other process uses.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{nanos}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = open_private(&tmp, true)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn read_history(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().map(unescape).collect())
        .unwrap_or_default()
}

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some('n')) => {
                out.push('\n');
                chars.next();
            }
            ('\\', Some('\\')) => {
                out.push('\\');
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_input_reads_lines_then_ctrl_d() {
        let mut input = std::io::Cursor::new(b"hello\r\n/help\n".to_vec());
        let mut out = Vec::new();
        let read = |i: &mut std::io::Cursor<Vec<u8>>, o: &mut Vec<u8>| {
            super::read_plain(i, o, "> ").unwrap()
        };
        assert_eq!(
            read(&mut input, &mut out),
            super::Line::Text("hello".into())
        );
        assert_eq!(
            read(&mut input, &mut out),
            super::Line::Text("/help".into())
        );
        assert_eq!(read(&mut input, &mut out), super::Line::CtrlD);
        assert_eq!(out, b"> > > ");
    }

    use super::*;

    #[test]
    fn history_keeps_the_last_entries_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let h = HistoryFile::new(Some(path.clone()));
        for i in 0..HISTORY_ENTRIES + 5 {
            h.add(&format!("m{i}"));
        }
        h.add("a\\b\nc");
        h.add("a\\b\nc");
        let back = read_history(&path);
        assert_eq!(back.len(), HISTORY_ENTRIES);
        assert_eq!(back[0], "m6");
        assert_eq!(back.last().unwrap(), "a\\b\nc");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Nothing but the file and its lock is left behind.
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["h", "h.lock"]);
    }

    /// Review T30-M6: REPLs writing at once lose no entry.
    #[test]
    fn concurrent_writers_keep_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let h = HistoryFile::new(Some(path));
                    for i in 0..20 {
                        h.add(&format!("t{t}-{i}"));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(read_history(&path).len(), 160);
    }
}
