use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tautly_context::agents_md::{self, AgentsMd, InstructionFile};
use tautly_context::{Context, PinSource};
use tautly_provider::{CacheStyle, Capabilities, EditFormat, ReasoningStyle};
use tautly_sandbox::LocalSandbox;

fn load(root: &Path) -> AgentsMd {
    agents_md::load(&LocalSandbox::new(root.to_path_buf()).unwrap())
}

/// `load` on a thread, or None when it has not returned within two seconds.
fn load_within_2s(root: &Path) -> Option<AgentsMd> {
    let sb = LocalSandbox::new(root.to_path_buf()).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(agents_md::load(&sb));
    });
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn caps() -> Capabilities {
    Capabilities {
        context_window: 100_000,
        max_output: 8_000,
        parallel_tools: true,
        reasoning: ReasoningStyle::None,
        cache: CacheStyle::Explicit,
        images: false,
        edit_format: EditFormat::StrReplace,
    }
}

#[test]
fn agents_md_rules_section_autopinned() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "# Project\nhello\n\n## Rules\n- never touch migrations/\n",
    )
    .unwrap();
    let found = load(dir.path());
    assert!(found.text.unwrap().contains("hello"));
    assert_eq!(found.pins.len(), 1);
    assert_eq!(found.pins[0].source, PinSource::AgentsMd);
    assert_eq!(found.pins[0].text, "- never touch migrations/");
    assert!(found.warnings.is_empty());
}

#[test]
fn agents_md_heading_matching_and_section_extent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "# Constraints\n- a\n\n### sub\n- b\n\n# Style\n- not pinned\n\n## RULES\n- c\n\n## Rules and more\n- not this\n\n```\n# Rules\n- in a fence\n```\n",
    )
    .unwrap();
    let pins = load(dir.path()).pins;
    let texts: Vec<&str> = pins.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, ["- a\n\n### sub\n- b", "- c"]);
}

#[test]
fn agents_md_concatenates_ancestors_nearest_last() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("pkg");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "outer text\n## Rules\n- outer rule\n",
    )
    .unwrap();
    std::fs::write(
        sub.join("AGENTS.md"),
        "inner text\n## Rules\n- inner rule\n",
    )
    .unwrap();
    let found = load(&sub);
    let text = found.text.unwrap();
    assert!(text.find("outer text").unwrap() < text.find("inner text").unwrap());
    let texts: Vec<&str> = found.pins.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(texts, ["- outer rule", "- inner rule"]);
}

#[test]
fn agents_md_absent_gives_none() {
    let dir = tempfile::tempdir().unwrap();
    // No AGENTS.md in the temp dir or (in practice) any of its ancestors.
    let found = load(dir.path());
    assert_eq!(found, AgentsMd::default());
}

#[test]
fn agents_md_reads_each_file_once_and_dedupes_pins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pkg")).unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "outer text\n## Rules\n- same rule\n",
    )
    .unwrap();
    // `pkg/..` and its ancestors name the same file twice unless paths are normalized.
    let found = load(&dir.path().join("pkg").join(".."));
    assert_eq!(found.text.unwrap().matches("outer text").count(), 1);
    assert_eq!(found.pins.len(), 1);

    // The same rule in two files is pinned once.
    std::fs::write(
        dir.path().join("pkg").join("AGENTS.md"),
        "## Rules\n- same rule\n",
    )
    .unwrap();
    assert_eq!(load(&dir.path().join("pkg")).pins.len(), 1);
}

#[test]
fn agents_md_accepts_headings_indented_up_to_three_spaces() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "   ## Rules\n- indented ok\n\n    ## Rules\n- four spaces is code\n",
    )
    .unwrap();
    let pins = load(dir.path()).pins;
    let texts: Vec<&str> = pins.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(
        texts,
        ["- indented ok\n\n    ## Rules\n- four spaces is code"]
    );
}

// ---- confinement and caps (C1) ----

#[test]
fn symlink_out_of_the_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    let root = dir.path().join("root");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        outside.join("credentials"),
        "## Rules\nfake_key = TEST-NOT-A-SECRET\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("../outside/credentials", root.join("AGENTS.md")).unwrap();

    let found = load(&root);
    assert_eq!(found.text, None, "{found:?}");
    assert!(found.pins.is_empty());
    assert!(
        found.warnings.iter().any(|w| w.contains("outside")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn symlink_inside_the_root_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(
        dir.path().join("docs").join("agents.md"),
        "linked text\n## Rules\n- linked rule\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("docs/agents.md", dir.path().join("AGENTS.md")).unwrap();

    let found = load(dir.path());
    assert!(found.text.unwrap().contains("linked text"));
    assert_eq!(found.pins[0].text, "- linked rule");
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn fifo_is_skipped_promptly() {
    let dir = tempfile::tempdir().unwrap();
    mkfifo(&dir.path().join("AGENTS.md"));
    let found = load_within_2s(dir.path()).expect("load returned within 2 s");
    assert_eq!(found.text, None);
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.contains("not a regular file")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn fifo_in_an_ancestor_is_skipped_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("AGENTS.md"), "inner text\n").unwrap();
    mkfifo(&dir.path().join("AGENTS.md"));
    let found = load_within_2s(&root).expect("load returned within 2 s");
    assert_eq!(found.text.as_deref(), Some("inner text"));
}

#[test]
fn ancestor_may_link_within_its_own_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let dotfiles = dir.path().join("dotfiles");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&dotfiles).unwrap();
    std::fs::write(dotfiles.join("mine.md"), "the user's own notes\n").unwrap();
    // A stow-style link that stays under the directory holding the AGENTS.md.
    std::os::unix::fs::symlink(dotfiles.join("mine.md"), dir.path().join("AGENTS.md")).unwrap();
    let found = load(&root);
    assert_eq!(found.text.as_deref(), Some("the user's own notes"));
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn ancestor_link_out_of_its_directory_is_refused() {
    // `git clone evil && cd evil/pkg && tautly`: the clone's top-level AGENTS.md is an
    // "ancestor" of the root but just as repository-controlled.
    let dir = tempfile::tempdir().unwrap();
    let clone = dir.path().join("clone");
    let root = clone.join("pkg");
    let home_secret = dir.path().join("home_secret");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(clone.join(".git")).unwrap();
    std::fs::create_dir(&home_secret).unwrap();
    std::fs::write(
        home_secret.join("credentials"),
        "fake_key = TEST-NOT-A-SECRET\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("../home_secret/credentials", clone.join("AGENTS.md")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "package notes\n").unwrap();

    let found = load(&root);
    let text = found.text.unwrap_or_default();
    assert!(!text.contains("TEST-NOT-A-SECRET"), "{text}");
    assert_eq!(text, "package notes");
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.contains("outside the directory")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn in_root_link_to_a_protected_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(
        dir.path().join(".git").join("config"),
        "[remote \"origin\"]\n\turl = https://TEST-TOKEN@example.invalid/x.git\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(".git/config", dir.path().join("AGENTS.md")).unwrap();
    let found = load(dir.path());
    assert_eq!(found.text, None, "{found:?}");
    assert!(
        found.warnings.iter().any(|w| w.contains("protected")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn rules_pins_are_capped_each_and_in_total() {
    let dir = tempfile::tempdir().unwrap();
    let section = |c: char| format!("## Rules\n{}\n", c.to_string().repeat(3_000));
    std::fs::write(
        dir.path().join("AGENTS.md"),
        format!("intro\n{}{}{}", section('a'), section('b'), section('c')),
    )
    .unwrap();
    let found = load(dir.path());
    // The full sections stay in the <agents_md> text.
    assert!(found.text.as_ref().unwrap().contains(&"c".repeat(3_000)));
    // Pins: 2 KB each at most, 4 KB together; the third does not fit.
    assert_eq!(
        found.pins.len(),
        2,
        "{:?}",
        found.pins.iter().map(|p| p.text.len()).collect::<Vec<_>>()
    );
    for p in &found.pins {
        assert!(p.text.starts_with(&"a".repeat(10)) || p.text.starts_with(&"b".repeat(10)));
        assert!(
            p.text.contains("truncated"),
            "{}",
            &p.text[p.text.len() - 80..]
        );
        assert!(p.text.len() <= 2_048 + 100, "{}", p.text.len());
    }
    let pinned: usize = found.pins.iter().map(|p| p.text.len()).sum();
    assert!(pinned <= 4_096 + 200, "{pinned}");
    assert!(
        found.warnings.iter().any(|w| w.contains("pinned")),
        "{:?}",
        found.warnings
    );
}

#[test]
fn oversize_file_is_truncated_with_a_marker_and_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    // 100 KB of text; the per-file cap is 32 KB.
    let line = "use tabs, not spaces, in every file of this repository\n";
    let body = line.repeat(100 * 1024 / line.len());
    std::fs::write(dir.path().join("AGENTS.md"), &body).unwrap();

    let found = load(dir.path());
    let text = found.text.clone().unwrap();
    assert!(
        text.ends_with("[AGENTS.md truncated at 32 KB]"),
        "{}",
        &text[text.len() - 80..]
    );
    assert!(text.len() <= 32 * 1024 + 64, "{}", text.len());
    assert!(text.starts_with(line.trim_end()));
    assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    assert!(found.warnings[0].contains("32 KB"), "{:?}", found.warnings);

    // The warning reaches every status.
    let mut c = Context::default();
    c.set_agents_md(found);
    let (_, st) = c.assemble(&caps(), &[], &"anthropic/claude-sonnet".into());
    assert!(
        st.warnings.iter().any(|w| w.contains("AGENTS.md")),
        "{:?}",
        st.warnings
    );
}

#[test]
fn total_is_capped_keeping_the_nearest_files_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mid = dir.path().join("a");
    let root = mid.join("b");
    std::fs::create_dir_all(&root).unwrap();
    // Three files of 30 KB each: 90 KB against a 64 KB total.
    let file = |tag: &str| format!("{tag}\n{}", "x".repeat(30 * 1024 - tag.len() - 1));
    std::fs::write(dir.path().join("AGENTS.md"), file("OUTER")).unwrap();
    std::fs::write(mid.join("AGENTS.md"), file("MIDDLE")).unwrap();
    std::fs::write(root.join("AGENTS.md"), file("NEAREST")).unwrap();

    let found = load(&root);
    let text = found.text.unwrap();
    assert!(text.len() <= 64 * 1024 + 128, "{}", text.len());
    // Nearest last, and the nearest two whole; the outermost is cut to what is left.
    let outer = text.find("OUTER").unwrap();
    let middle = text.find("MIDDLE").unwrap();
    let nearest = text.find("NEAREST").unwrap();
    assert!(outer < middle && middle < nearest);
    assert!(text[outer..middle].contains("[AGENTS.md truncated at 4 KB]"));
    assert!(text[nearest..].len() >= 30 * 1024 - 1);
    assert!(
        found.warnings.iter().any(|w| w.contains("64 KB")),
        "{:?}",
        found.warnings
    );
}

// ---- ancestor ownership and modes (D6) ----

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A temp dir holding `AGENTS.md` (mode `file_mode`) and the sandbox root `root/`, the
/// holding directory set to `dir_mode`.
fn ancestor_case(file_mode: u32, dir_mode: u32) -> (tempfile::TempDir, AgentsMd) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let file = dir.path().join("AGENTS.md");
    std::fs::write(&file, "ancestor notes\n").unwrap();
    chmod(&file, file_mode);
    chmod(dir.path(), dir_mode);
    let found = load(&root);
    chmod(dir.path(), 0o755);
    (dir, found)
}

#[test]
fn own_ancestor_file_in_a_private_directory_is_accepted() {
    let (_d, found) = ancestor_case(0o644, 0o755);
    assert_eq!(found.text.as_deref(), Some("ancestor notes"));
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn ancestor_file_writable_by_others_is_refused() {
    for mode in [0o666, 0o664, 0o646] {
        let (_d, found) = ancestor_case(mode, 0o755);
        assert_eq!(found.text, None, "mode {mode:o}");
        assert!(
            found.warnings.iter().any(|w| w.contains("writable")),
            "mode {mode:o}: {:?}",
            found.warnings
        );
    }
}

#[test]
fn ancestor_in_a_world_writable_directory_needs_the_sticky_bit() {
    let (_d, found) = ancestor_case(0o644, 0o777);
    assert_eq!(found.text, None);
    assert!(
        found.warnings.iter().any(|w| w.contains("world-writable")),
        "{:?}",
        found.warnings
    );
    // Sticky (like /tmp) and the file is the current user's: accepted.
    let (_d, found) = ancestor_case(0o644, 0o1777);
    assert_eq!(found.text.as_deref(), Some("ancestor notes"));
}

#[test]
fn the_roots_own_file_is_not_subject_to_the_ancestor_rule() {
    // A group-writable file inside the project is the repository's, read as before.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("AGENTS.md");
    std::fs::write(&file, "repo notes\n").unwrap();
    chmod(&file, 0o664);
    assert_eq!(load(dir.path()).text.as_deref(), Some("repo notes"));
}

// ---- CLAUDE.md fallback (spec 7.11.6) ----

fn canon(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap()
}

#[test]
fn claude_md_used_when_no_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "claude notes\n").unwrap();
    let found = load(dir.path());
    assert!(found.text.as_deref().unwrap().contains("claude notes"));
    assert_eq!(
        found.sources,
        [(
            canon(dir.path()).join("CLAUDE.md"),
            InstructionFile::ClaudeMd
        )]
    );
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn agents_md_wins_when_both_exist() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "outer claude\n").unwrap();
    std::fs::write(root.join("AGENTS.md"), "agents notes\n## Rules\n- a rule\n").unwrap();
    std::fs::write(root.join("CLAUDE.md"), "claude notes\n## Rules\n- c rule\n").unwrap();
    let found = load(&root);
    let text = found.text.clone().unwrap();
    // The root level reads only AGENTS.md; the outer level has no AGENTS.md and falls back.
    assert!(
        text.contains("agents notes") && text.contains("outer claude"),
        "{text}"
    );
    assert!(!text.contains("claude notes"), "{text}");
    assert_eq!(
        found.sources,
        [
            (
                canon(dir.path()).join("CLAUDE.md"),
                InstructionFile::ClaudeMd
            ),
            (canon(&root).join("AGENTS.md"), InstructionFile::AgentsMd),
        ]
    );
    let pins: Vec<&str> = found.pins.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(pins, ["- a rule"]);
}

#[test]
fn an_unusable_agents_md_still_wins_over_claude_md() {
    // "Where both files exist, only AGENTS.md is read": a refused AGENTS.md is not a
    // reason to trust the other file instead.
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    let root = dir.path().join("root");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(outside.join("x"), "elsewhere\n").unwrap();
    std::os::unix::fs::symlink("../outside/x", root.join("AGENTS.md")).unwrap();
    std::fs::write(root.join("CLAUDE.md"), "claude notes\n").unwrap();
    let found = load(&root);
    assert_eq!(found.text, None, "{found:?}");
    assert!(found.sources.is_empty());
    assert!(found.warnings.iter().any(|w| w.contains("outside")));
}

#[test]
fn claude_md_obeys_caps_confinement_and_ownership() {
    // Confinement: a link out of the root is refused.
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    let root = dir.path().join("root");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        outside.join("credentials"),
        "fake_key = TEST-NOT-A-SECRET\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("../outside/credentials", root.join("CLAUDE.md")).unwrap();
    let found = load(&root);
    assert_eq!(found.text, None, "{found:?}");
    assert!(found.sources.is_empty());
    assert!(
        found.warnings.iter().any(|w| w.contains("outside")),
        "{:?}",
        found.warnings
    );

    // ... a link to a protected path too.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git").join("config"), "[core]\n").unwrap();
    std::os::unix::fs::symlink(".git/config", dir.path().join("CLAUDE.md")).unwrap();
    let found = load(dir.path());
    assert_eq!(found.text, None, "{found:?}");
    assert!(found.warnings.iter().any(|w| w.contains("protected")));

    // ... an in-root link works.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs").join("c.md"), "linked claude\n").unwrap();
    std::os::unix::fs::symlink("docs/c.md", dir.path().join("CLAUDE.md")).unwrap();
    assert!(load(dir.path()).text.unwrap().contains("linked claude"));

    // Ownership and modes above the root: group-writable file, world-writable directory.
    for (file_mode, dir_mode) in [(0o664, 0o755), (0o644, 0o777)] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let file = dir.path().join("CLAUDE.md");
        std::fs::write(&file, "ancestor claude\n").unwrap();
        chmod(&file, file_mode);
        chmod(dir.path(), dir_mode);
        let found = load(&root);
        chmod(dir.path(), 0o755);
        assert_eq!(found.text, None, "modes {file_mode:o}/{dir_mode:o}");
        assert!(
            found.warnings.iter().any(|w| w.contains("writable")),
            "{:?}",
            found.warnings
        );
    }
    // A clean ancestor CLAUDE.md is accepted, and a link out of its directory is not.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "ancestor claude\n").unwrap();
    assert!(load(&root).text.unwrap().contains("ancestor claude"));
    let dir = tempfile::tempdir().unwrap();
    let clone = dir.path().join("clone");
    let root = clone.join("pkg");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(dir.path().join("secret"), "TEST-NOT-A-SECRET\n").unwrap();
    std::os::unix::fs::symlink("../secret", clone.join("CLAUDE.md")).unwrap();
    let found = load(&root);
    assert_eq!(found.text, None, "{found:?}");
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.contains("outside the directory"))
    );

    // Caps: 40 KB is cut at 32 KB with the marker (naming the file) and a warning.
    let dir = tempfile::tempdir().unwrap();
    let line = "use tabs, not spaces, in every file of this repository\n";
    std::fs::write(
        dir.path().join("CLAUDE.md"),
        line.repeat(40 * 1024 / line.len()),
    )
    .unwrap();
    let found = load(dir.path());
    let text = found.text.unwrap();
    assert!(
        text.ends_with("[CLAUDE.md truncated at 32 KB]"),
        "{}",
        &text[text.len() - 60..]
    );
    assert!(text.len() <= 32 * 1024 + 128, "{}", text.len());
    assert!(
        found.warnings.iter().any(|w| w.contains("32 KB")),
        "{:?}",
        found.warnings
    );

    // The 64 KB total is shared with AGENTS.md files: 30 KB of each, three levels.
    let dir = tempfile::tempdir().unwrap();
    let mid = dir.path().join("a");
    let root = mid.join("b");
    std::fs::create_dir_all(&root).unwrap();
    let file = |tag: &str| format!("{tag}\n{}", "x".repeat(30 * 1024 - tag.len() - 1));
    std::fs::write(dir.path().join("CLAUDE.md"), file("OUTER")).unwrap();
    std::fs::write(mid.join("AGENTS.md"), file("MIDDLE")).unwrap();
    std::fs::write(root.join("CLAUDE.md"), file("NEAREST")).unwrap();
    let found = load(&root);
    let text = found.text.unwrap();
    assert!(text.len() <= 64 * 1024 + 256, "{}", text.len());
    assert!(
        text[text.find("OUTER").unwrap()..text.find("MIDDLE").unwrap()]
            .contains("[CLAUDE.md truncated at 4 KB]")
    );
    assert!(
        found.warnings.iter().any(|w| w.contains("64 KB")),
        "{:?}",
        found.warnings
    );

    // A FIFO named CLAUDE.md does not hang start-up.
    let dir = tempfile::tempdir().unwrap();
    mkfifo(&dir.path().join("CLAUDE.md"));
    let found = load_within_2s(dir.path()).expect("load returned within 2 s");
    assert_eq!(found.text, None);
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.contains("not a regular file"))
    );
}

#[test]
fn claude_md_rules_section_auto_pinned() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("CLAUDE.md"),
        "# Project\nhello\n\n## Rules\n- never touch migrations/\n\n## Constraints\n- no network\n",
    )
    .unwrap();
    let found = load(dir.path());
    let pins: Vec<&str> = found.pins.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(pins, ["- never touch migrations/", "- no network"]);
    assert!(found.pins.iter().all(|p| p.source == PinSource::AgentsMd));
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn claude_md_at_imports_kept_as_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.md"), "IMPORTED-CONTENT\n").unwrap();
    std::fs::write(
        dir.path().join("CLAUDE.md"),
        "see below\n@secret.md\n@~/.ssh/config\n",
    )
    .unwrap();
    let found = load(dir.path());
    let text = found.text.unwrap();
    assert!(
        text.contains("@secret.md") && text.contains("@~/.ssh/config"),
        "{text}"
    );
    assert!(!text.contains("IMPORTED-CONTENT"), "{text}");
    assert!(found.warnings.is_empty(), "{:?}", found.warnings);
}

#[test]
fn block_header_names_file_used() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "claude notes\n").unwrap();
    let found = load(dir.path());
    let text = found.text.clone().unwrap();
    assert!(
        text.starts_with("[instructions from CLAUDE.md]\n"),
        "{text}"
    );
    // The header is part of the `<agents_md>` block the model sees.
    let mut c = Context::default();
    c.set_agents_md(found);
    let (req, _) = c.assemble(&caps(), &[], &"anthropic/claude-sonnet".into());
    let block = format!("{:?}", req.system[1]);
    assert!(block.contains("instructions from CLAUDE.md"), "{block}");

    // With an AGENTS.md at the same level, only it is read and no header names CLAUDE.md.
    std::fs::write(dir.path().join("AGENTS.md"), "agents notes\n").unwrap();
    assert_eq!(load(dir.path()).text.unwrap(), "agents notes");

    // Mixed levels: only the CLAUDE.md level is labelled.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pkg");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "outer agents\n").unwrap();
    std::fs::write(root.join("CLAUDE.md"), "inner claude\n").unwrap();
    assert_eq!(
        load(&root).text.unwrap(),
        "outer agents\n\n[instructions from CLAUDE.md]\ninner claude"
    );
}
