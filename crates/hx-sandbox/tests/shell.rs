use hx_sandbox::{Redirect, SimpleCommand, parse_commands};

fn argv0s(c: &[SimpleCommand]) -> Vec<&str> {
    c.iter().map(|s| s.argv[0].as_str()).collect()
}

#[test]
fn splits_pipes_and_lists() {
    let c = parse_commands("cargo test && git status | head -n 5; ls").unwrap();
    assert_eq!(argv0s(&c), ["cargo", "git", "head", "ls"]);
}

#[test]
fn descends_into_subshell_and_substitution() {
    let c = parse_commands("echo $(rm -rf /tmp/x) && (cd a && make)").unwrap();
    assert_eq!(argv0s(&c), ["echo", "rm", "cd", "make"]);
}

#[test]
fn captures_write_redirects() {
    let c = parse_commands("echo hi > .git/hooks/pre-commit").unwrap();
    assert_eq!(
        c[0].redirects,
        vec![Redirect {
            target: ".git/hooks/pre-commit".into(),
            write: true
        }]
    );
}

#[test]
fn unbalanced_quote_is_unparseable() {
    assert!(parse_commands("echo 'oops").is_err());
}

#[test]
fn eval_and_bash_dash_c_are_unparseable() {
    assert!(parse_commands("eval \"$X\"").is_err());
    assert!(parse_commands("bash -c 'rm -rf /'").is_err());
}

#[test]
fn source_and_dot_are_unparseable() {
    assert!(parse_commands("source ./x.sh").is_err());
    assert!(parse_commands(". ./x.sh").is_err());
    assert!(parse_commands("sh -c 'ls'").is_err());
    assert!(parse_commands("zsh -lc 'ls'").is_err());
}

#[test]
fn unquotes_words_and_keeps_variables_literal() {
    let c = parse_commands(r#"echo "a b" 'c d' e\ f $X "$Y/z""#).unwrap();
    assert_eq!(c[0].argv, ["echo", "a b", "c d", "e f", "$X", "$Y/z"]);
}

#[test]
fn append_and_stderr_redirects_are_writes_dups_are_not() {
    let c = parse_commands("cmd >> a.log 2> err.log >&2 2>&1 &> both.log").unwrap();
    let t: Vec<_> = c[0]
        .redirects
        .iter()
        .map(|r| (r.target.as_str(), r.write))
        .collect();
    assert_eq!(t, [("a.log", true), ("err.log", true), ("both.log", true)]);
}

#[test]
fn input_redirects_heredocs_herestrings_are_not_writes() {
    let c = parse_commands("cat < in.txt").unwrap();
    assert_eq!(
        c[0].redirects,
        vec![Redirect {
            target: "in.txt".into(),
            write: false
        }]
    );
    let c = parse_commands("cat <<EOF\nhello > not_a_file\nEOF\n").unwrap();
    assert!(c[0].redirects.iter().all(|r| !r.write));
    let c = parse_commands("cat <<< hi").unwrap();
    assert!(c[0].redirects.iter().all(|r| !r.write));
}

#[test]
fn wrappers_are_not_unwrapped() {
    let c = parse_commands("sudo env FOO=1 time nohup xargs rm").unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(
        c[0].argv,
        ["sudo", "env", "FOO=1", "time", "nohup", "xargs", "rm"]
    );
}

#[test]
fn process_substitution_and_nested_forms() {
    let c = parse_commands(
        "diff <(ls a) <(ls b); if true; then rm x; fi; for i in 1; do echo $i; done",
    )
    .unwrap();
    assert_eq!(argv0s(&c), ["diff", "ls", "ls", "true", "rm", "echo"]);
}

#[test]
fn redirect_on_compound_commands_is_captured() {
    let c = parse_commands("(echo a; echo b) > out.txt").unwrap();
    assert_eq!(argv0s(&c), ["echo", "echo"]);
    assert!(
        c.iter()
            .all(|s| s.redirects.iter().any(|r| r.target == "out.txt" && r.write))
    );
}

#[test]
fn empty_and_assignment_only() {
    assert!(parse_commands("").unwrap().is_empty());
    assert!(parse_commands("FOO=1").unwrap().is_empty());
}
