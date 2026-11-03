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

// ---- fix round: fail-closed behaviour ----

#[test]
fn shell_dash_c_with_intervening_options_is_unparseable() {
    for s in [
        "bash --norc -c 'rm x'",
        "bash -o pipefail -c 'rm x'",
        "bash -O extglob -c 'rm x'",
        "bash --rcfile f -c 'rm x'",
        "bash +O x -c 'rm x'",
        "/bin/bash -ec 'rm x'",
        "bash --command 'rm x'",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    assert!(parse_commands("bash script.sh").is_ok());
}

#[test]
fn declaration_unset_and_test_commands_are_reported() {
    let c = parse_commands("declare -x A=1").unwrap();
    assert_eq!(c[0].argv, ["declare", "-x", "A=1"]);
    assert_eq!(
        parse_commands("local x=1").unwrap()[0].argv,
        ["local", "x=1"]
    );
    assert_eq!(parse_commands("unset X").unwrap()[0].argv, ["unset", "X"]);
    assert_eq!(parse_commands("[[ -f x ]]").unwrap()[0].argv, ["[["]);
    let c = parse_commands("export X=$(id)").unwrap();
    assert_eq!(c[0].argv, ["export", "X=$(id)"]);
    assert_eq!(argv0s(&c), ["export", "id"]);
    let c = parse_commands("[[ $(whoami) == x ]]").unwrap();
    assert_eq!(argv0s(&c), ["[[", "whoami"]);
}

#[test]
fn zero_commands_from_nonempty_input_is_unparseable() {
    assert!(parse_commands("((x=1))").is_err());
    assert!(parse_commands("A=1; B=2").unwrap().is_empty());
    assert!(parse_commands("# just a comment").unwrap().is_empty());
}

#[test]
fn function_definition_redirect_is_a_write() {
    let c = parse_commands("f() { :; } > out.txt").unwrap();
    assert!(
        c.iter()
            .all(|s| s.redirects.iter().any(|r| r.target == "out.txt" && r.write))
    );
}

#[test]
fn heredoc_with_output_redirect_is_a_write() {
    let c = parse_commands("cat <<EOF > o.txt\nx\nEOF\n").unwrap();
    assert_eq!(
        c[0].redirects,
        vec![Redirect {
            target: "o.txt".into(),
            write: true
        }]
    );
    let c = parse_commands("cat <<EOF | tee y\nx\nEOF\n").unwrap();
    assert_eq!(argv0s(&c), ["cat", "tee"]);
}

#[test]
fn write_redirect_without_command_is_unparseable() {
    assert!(parse_commands("> f").is_err());
    assert!(parse_commands("A=1 > f").is_err());
}

#[test]
fn redirect_without_target_is_unparseable_except_fd_close() {
    assert!(parse_commands("ls >&-").unwrap()[0].redirects.is_empty());
    assert!(parse_commands("ls 2>&-").unwrap()[0].redirects.is_empty());
}

#[test]
fn dynamic_command_names_are_unparseable() {
    assert!(parse_commands("$CMD -rf /").is_err());
    assert!(parse_commands("\"$(printf rm)\" x").is_err());
    assert!(parse_commands("`echo rm` x").is_err());
}

#[test]
fn eval_and_shell_c_are_found_through_wrappers() {
    for s in [
        "sudo bash -c 'x'",
        "sudo -u root sh -c x",
        "command eval x",
        "builtin eval x",
        "exec bash -c x",
        "env -i sh -c x",
        "env FOO=1 bash -c x",
        "nohup bash -c x",
        "time bash -c x",
        "xargs -n1 sh -c x",
        "sudo env FOO=1 source f",
        "find . -exec sh -c x {} ;",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
}

#[test]
fn trap_and_coproc_are_unparseable() {
    assert!(parse_commands("trap 'rm x' EXIT").is_err());
    assert!(parse_commands("coproc cat").is_err());
}

// ---- checkpoint fix: C2 leading environment assignments ----

#[test]
fn leading_assignments_are_kept_verbatim() {
    let c = parse_commands("FOO=1 BAR='x y' ls -l").unwrap();
    assert_eq!(c[0].assignments, ["FOO=1", "BAR='x y'"]);
    assert_eq!(c[0].argv, ["ls", "-l"]);
    let c = parse_commands("ls; A=$(id) git status").unwrap();
    assert!(c[0].assignments.is_empty());
    assert_eq!(c[1].assignments, ["A=$(id)"]);
    assert_eq!(argv0s(&c), ["ls", "git", "id"]);
}

#[test]
fn bare_assignment_to_dangerous_variable_is_unparseable() {
    for s in [
        "PATH=/tmp/evil; ls",
        "LD_PRELOAD=./x.so; export LD_PRELOAD; ls",
        "GIT_DIR=x",
        "IFS=/",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    assert!(parse_commands("FOO=1; ls").is_ok());
}

// ---- checkpoint fix: C3 hidden command substitution, I4 nesting depth ----

#[test]
fn arithmetic_and_subscripts_on_variables_are_unparseable() {
    // bash evaluates variable values inside arithmetic and subscripts as code.
    for s in [
        "x='a[$(touch /tmp/pwn)]'; echo $((x))",
        "echo $((x+1))",
        "echo $[x]",
        "((x)); ls",
        "for ((i=0;i<n;i++)); do echo; done",
        "echo ${a[$i]}",
        "echo ${a[i]}",
        "a[$i]=1",
        "a=([$x]=1)",
        "echo ${!x}",
        "echo ${s:o}",
        "[[ $x -eq 1 ]]",
        "[[ x -lt 2 ]]",
        "[[ -v $x ]]",
        "test -v \"$x\"",
        "[ -v \"$x\" ]",
        "echo $((a[$(id)]))",
        "let x=1",
        "declare -i n=x",
        "declare -n r=x",
        "local -i n",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "echo $((1+2))",
        "echo $((0x1f))",
        "[[ $? -eq 0 ]]",
        "[[ $# -gt 1 ]]",
        "echo ${a[0]} ${#x} ${x:-y} ${x:1:2}",
        "[[ -f x && $y == z ]]",
        "a[1]=2",
        "x=(a b)",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn variable_name_arguments_with_code_are_unparseable() {
    for s in [
        "echo x | read 'a[$(touch pwn4)]'",
        "read \"$x\"",
        "read -r -a \"$x\"",
        "unset 'a[$(id)]'",
        "unset \"$x\"",
        "printf -v 'a[$(id)]' %s 1",
        "printf -v \"$x\" %s 1",
        "export \"$x\"",
        "declare \"$x\"=1",
        "mapfile -C cb arr",
        "readarray -t \"$x\"",
        "getopts ab \"$x\"",
        "wait -p \"$x\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "read -r line",
        "read -p \"$prompt\" -r x",
        "printf '%s\\n' \"$x\" 'a[$(id)]'",
        "unset X",
        "export X=$(id)",
        "mapfile -t lines",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn deeply_nested_input_is_unparseable_not_a_crash() {
    let n = 20_000;
    for s in [
        format!("{}true{}", "(".repeat(n), ")".repeat(n)),
        format!("echo {}x{}", "$(".repeat(n), ")".repeat(n)),
        format!("{}true{}", "{ ".repeat(n), "; }".repeat(n)),
        format!("{}:{}", "if true; then ".repeat(n), "; fi".repeat(n)),
        format!("echo $(({}1{}))", "(".repeat(n), ")".repeat(n)),
    ] {
        assert!(parse_commands(&s).is_err());
    }
    // Moderate nesting still parses.
    let ok = format!("{}true{}", "( ".repeat(50), " )".repeat(50));
    assert_eq!(argv0s(&parse_commands(&ok).unwrap()), ["true"]);
}

// ---- checkpoint fix: T2 interpreters fed unseen code, wrapper look-through ----

#[test]
fn interpreters_running_unseen_code_are_unparseable() {
    for s in [
        "bash <<< 'rm x'",
        "bash <<EOF\nrm x\nEOF\n",
        "sh -s",
        "bash -s arg",
        "bash",
        "bash -",
        "bash -x",
        "cat x | bash",
        "curl x | sh",
        "bash < script.sh",
        "zsh <<< x",
        "python -c 'x'",
        "python3 -c x",
        "python3 -Ic x",
        "python -",
        "python3",
        "python3 <<EOF\nimport os\nEOF\n",
        "cat f | python3",
        "python3.12 -c x",
        "node -e x",
        "node --eval x",
        "node --eval=x",
        "node -p x",
        "node <<< x",
        "perl -e x",
        "perl -E x",
        "perl -we x",
        "perl < a",
        "ruby -e x",
        "ruby <<< x",
        "sudo python3 -c x",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "bash script.sh",
        "bash -x script.sh",
        "bash -o pipefail script.sh",
        "python3 script.py",
        "python3 -u script.py < in.txt",
        "python -m pytest -q",
        "python3 -W ignore x.py",
        "node index.js",
        "perl x.pl",
        "ruby x.rb",
        "python3 --version",
        "node --version",
        "bash --version",
        "cat x | python3 script.py",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn wrappers_with_dynamic_or_hidden_commands_are_unparseable() {
    for s in [
        "sudo $CMD",
        "sudo \"$CMD\" x",
        "timeout 5 $X",
        "nohup `echo rm` x",
        "env -S 'bash -c x'",
        "env -iS 'x'",
        "env --split-string='bash -c x'",
        "env --split-string 'x'",
        "stdbuf -o0 bash -c 'rm x'",
        "ionice -c3 sh -c x",
        "flock /tmp/l bash -c x",
        "flock /tmp/l -c 'rm x'",
        "flock -c 'rm x' /tmp/l",
        "watch ls",
        "chroot / ls",
        "su -c 'rm x'",
        "su root",
        "script -c 'rm x'",
        "script out.log",
        "taskset 1 sh -c x",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "stdbuf -o0 ls",
        "ionice -c3 cargo build",
        "flock /tmp/l cargo test",
        "command -v python3",
        "timeout 5 cargo test",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}
