use tautly_sandbox::{Redirect, SimpleCommand, parse_commands};

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
    let c = parse_commands("sudo env FOO=1 time nohup nice rm").unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(
        c[0].argv,
        ["sudo", "env", "FOO=1", "time", "nohup", "nice", "rm"]
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

#[test]
fn backquotes_in_unquoted_heredoc_bodies_are_unparseable() {
    // The grammar keeps `...` in heredoc bodies as plain text, but bash runs it.
    for s in [
        "cat <<EOF\na `rm x` b\nEOF\n",
        "cat <<-EOF\n\t`rm x`\n\tEOF\n",
        "cat <<EOF\n${x:-`rm x`}\nEOF\n",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    // Quoted delimiters make the body literal.
    for s in [
        "cat <<'EOF'\n`rm x`\nEOF\n",
        "cat <<\"EOF\"\n`rm x`\nEOF\n",
        "cat <<\\EOF\n`rm x`\nEOF\n",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
    // Round 2 ruling: any `$` in an unquoted body fails closed (see below).
    assert!(parse_commands("cat <<EOF\n$(rm x)\nEOF\n").is_err());
}

// ---- checkpoint fix round 2 ----

#[test]
fn any_expansion_in_an_unquoted_heredoc_body_is_unparseable() {
    for s in [
        "echo='a[$(touch pwn)]'; cat <<XEOF\n$((echo))\nXEOF\n",
        "cat <<XEOF\n$(( echo ))\nXEOF\n",
        "cat <<XEOF\n$[echo]\nXEOF\n",
        "cat <<XEOF\n$((1))\nXEOF\n",
        "cat <<XEOF\n${a[$x]}\nXEOF\n",
        "cat <<XEOF\n${x@P}\nXEOF\n",
        "cat <<XEOF\nhome is $HOME\nXEOF\n",
        "cat <<XEOF > out.txt\n$(id)\nXEOF\n",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "cat <<'XEOF'\n$((echo)) $(id) $HOME\nXEOF\n",
        "cat <<\"XEOF\"\n$(id)\nXEOF\n",
        "cat <<XEOF\nplain text only\nXEOF\n",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn name_operands_assigning_dangerous_variables_are_unparseable() {
    for s in [
        "for PATH in ./evilbin; do ls; done",
        "for HOME in /tmp/evil; do git status; done",
        "select PATH in /tmp/evil; do ls; done",
        "for PS4 in '$(touch x)'; do set -x; ls; done",
        "read PATH <<< /tmp/evil; ls",
        "read -r IFS <<< x; ls",
        "read -a LD_LIBRARY_PATH < f",
        "printf -v PATH %s /tmp/evil; ls",
        "getopts a PATH; ls",
        "mapfile -t PATH < f",
        "readarray BASH_ENV < f",
        "export LD_PRELOAD; ls",
        "declare -x GIT_DIR",
        "local PATH",
        "readonly IFS",
        "typeset -x PYTHONPATH",
        "for XDG_CONFIG_HOME in /tmp; do git status; done",
        "for SHELLOPTS in x; do :; done",
        "for BASHOPTS in x; do :; done",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "for f in a b; do echo $f; done",
        "read -r line < f",
        "printf -v out %s x",
        "export FOO",
        "unset PATH_TMP",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn transformation_operators_in_expansions_are_unparseable() {
    for s in [
        "echo \"${x@P}\"",
        "echo ${x@P}",
        "echo ${a[@]@Q}",
        "echo ${x@E}",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    assert!(parse_commands("echo ${@} ${@:2} ${a[@]} ${#a[@]}").is_ok());
}

#[test]
fn interpreter_device_paths_process_substitution_and_unknown_options_are_unparseable() {
    for s in [
        "bash /dev/stdin <<< 'x'",
        "bash /dev/fd/0 <<< 'x'",
        "bash /proc/self/fd/0",
        "bash -O extglob -- /dev/stdin",
        "sh -e /dev/stdin",
        "env bash /dev/stdin",
        "cat x | bash /dev/stdin",
        "true | bash /dev/stdin",
        "bash <(echo x)",
        "python3 /dev/stdin <<XEOF\nimport os\nXEOF\n",
        "python3 -X dev /dev/stdin",
        "ruby /dev/stdin",
        "perl /dev/stdin",
        "node /dev/stdin",
        "bash \"$script\"",
        "perl -I lib <<< 'x'",
        "node -C dev <<< 'x'",
        "python3 -m code <<< 'import os'",
        "python3 -m code < f",
        "echo x | python3 -m code",
        "python3 -Z x.py",
        "bash -j x.sh",
        "node --frobnicate x.js",
        "timeout 5 python3 -m code <<< x",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "bash -o posix script.sh",
        "bash -ex script.sh",
        "bash --norc --noprofile script.sh",
        "python3 -u -B -X dev x.py",
        "python3 -m pytest -q",
        "python3 -OO x.py",
        "perl -Ilib -w x.pl",
        "perl -I lib x.pl",
        "node --inspect x.js",
        "node -r ts-node/register x.ts",
        "ruby -w x.rb",
        "cat x | python3 script.py",
        "python3 script.py < in.txt",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn more_wrappers_and_xargs_into_runners_are_unparseable() {
    for s in [
        "arch -arm64 bash -c 'x'",
        "xcrun bash -c x",
        "sandbox-exec -p '(version 1)' sh -c x",
        "systemd-run --user sh -c x",
        "setpriv --reuid 1 sh -c x",
        "prlimit --nofile=10 sh -c x",
        "fakeroot sh -c x",
        "doas sh -c x",
        "arch $CMD",
        "echo \"sh -c id\" | xargs env",
        "echo x | xargs sudo",
        "echo x | xargs -n1 bash",
        "echo x | xargs python3",
        "echo x | xargs nice",
        // Any `xargs`: its command line is completed from stdin (CP5 review M2).
        "echo a | xargs rm",
        "find src -name '*.o' | xargs rm -f",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    assert!(parse_commands("arch -arm64 cargo build").is_ok());
}

#[test]
fn builtins_that_load_or_run_code_are_unparseable() {
    for s in [
        "compgen -C 'touch x' x",
        "compgen -F f x",
        "compgen -A file -C x y",
        "enable -f ./evil.so evil",
        "bind -x '\"\\C-t\": touch x'",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in ["compgen -A file", "enable -n echo", "bind -l"] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

// ---- checkpoint fix round 3 ----

fn cwds(c: &SimpleCommand) -> Vec<String> {
    c.cwds.iter().map(|p| p.display().to_string()).collect()
}

#[test]
fn literal_cd_is_tracked_lexically() {
    let c = parse_commands("cd sub && cargo test").unwrap();
    assert_eq!(argv0s(&c), ["cd", "cargo"]);
    assert_eq!(cwds(&c[0]), [""]);
    assert_eq!(cwds(&c[1]), ["sub"]);
    // After `;` (or `||`) the cd may have failed, so both directories are possible.
    let c = parse_commands("cd sub; ls").unwrap();
    assert_eq!(cwds(&c[1]), ["", "sub"]);
    let c = parse_commands("cd a && cd b && ls").unwrap();
    assert_eq!(cwds(&c[2]), ["a/b"]);
    let c = parse_commands("pushd -P crates/x && cat src/lib.rs").unwrap();
    assert_eq!(cwds(&c[1]), ["crates/x"]);
    // Subshells, substitutions and pipeline elements do not leak their cd.
    let c = parse_commands("(cd sub; ls); pwd").unwrap();
    assert_eq!(cwds(&c[1]), ["", "sub"]);
    let c = parse_commands("(cd sub && ls); pwd").unwrap();
    assert_eq!(cwds(&c[1]), ["sub"]);
    assert_eq!(cwds(&c[2]), [""]);
    let c = parse_commands("echo $(cd sub); cd x | true; pwd").unwrap();
    assert_eq!(cwds(c.last().unwrap()), [""]);
    let c = parse_commands("cd /abs && ls").unwrap();
    assert_eq!(cwds(&c[1]), ["/abs"]);
}

#[test]
fn unanalyzable_directory_changes_followed_by_commands_are_unparseable() {
    for s in [
        "cd .g*/hooks; echo pwn > pre-commit",
        "pushd .g*/hooks; echo pwn > pre-commit",
        "cd $D; ls",
        "cd \"$D\" && ls",
        "cd `x` && ls",
        "cd ~/x; ls",
        "cd ~; ls",
        "cd -; ls",
        "cd; ls",
        "cd a b; ls",
        "popd; ls",
        "pushd; ls",
        "pushd +1; ls",
        "dirs -c; ls",
        "for d in a b; do cd sub; done; ls",
        "while true; do cd sub; done; ls",
        "f() { cd sub; }; f; ls",
        "(cd $D; ls)",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "cd $D",
        "(cd $D); ls",
        "cd sub && cargo test",
        "cd -P sub && ls",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn arithmetic_operands_of_builtins_are_unparseable() {
    // bash >= 4 evaluates `a[$(cmd)]` subscripts in these numeric operands.
    for s in [
        "test \"$x\" -eq 1",
        "test 1 -lt $x",
        "[ \"$x\" -gt 0 ]",
        "[ `x` -ne 0 ]",
        "shift \"$x\"",
        "ulimit -n \"$x\"",
        "return $x",
        "exit \"$x\"",
        "break $n",
        "continue $n",
        "printf '%d' \"$x\"",
        "printf '%5.2x\\n' $x",
        "printf '%i %s' 1 \"$x\"",
        "printf '%*s' \"$w\" x",
        "printf \"$fmt\" \"$x\"",
        "printf -v out '%u' \"$x\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "test 1 -eq 1",
        "test -f \"$x\"",
        "[ \"$a\" = \"$b\" ]",
        "shift 2",
        "exit 1",
        "ulimit -n 4096",
        "printf '%s\\n' \"$x\"",
        "printf '%d' 5",
        "printf '100%%\\n'",
        "printf '%d%%' 5",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

// ---- checkpoint fix round 4 ----

fn cwd_set(c: &SimpleCommand) -> std::collections::BTreeSet<String> {
    cwds(c).into_iter().collect()
}

#[test]
fn cdpath_assignments_are_unparseable() {
    for s in [
        "CDPATH=.git; cd hooks && echo pwn > pre-commit",
        "export CDPATH=:.git; ls",
        "printf -v CDPATH '.%s' git; ls",
        "read CDPATH <<< .git; ls",
        "for CDPATH in .git; do :; done; ls",
        "declare -x CDPATH=.git; ls",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
}

#[test]
fn cd_through_builtin_command_time_and_backslash_is_tracked() {
    for s in [
        "builtin cd sub && cat x",
        "command cd sub && cat x",
        "command -- cd sub && cat x",
        "command -p cd sub && cat x",
        "time cd sub && cat x",
        "time -p cd sub && cat x",
        "\\cd sub && cat x",
        "\\builtin cd sub && cat x",
        "builtin pushd sub && cat x",
    ] {
        let c = parse_commands(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        let cat = c.iter().find(|c| c.argv[0] == "cat").unwrap();
        assert_eq!(cwds(cat), ["sub"], "{s}");
    }
    for s in [
        "builtin cd .g*/hooks; echo pwn > pre-commit",
        "command cd $D; ls",
        "time cd .g*/hooks; echo x",
        "\\builtin cd ~; ls",
        "builtin popd; ls",
        "command dirs -c; ls",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
}

#[test]
fn shell_option_changes_followed_by_commands_are_unparseable() {
    for s in [
        "shopt -s lastpipe; true | cd .g*/hooks; echo pwn > pre-commit",
        "shopt -s lastpipe; ls",
        "shopt -u extglob; ls",
        "shopt -s -o posix; ls",
        "set -o posix; ls",
        "set +e; ls",
        "set -f; ls",
        "set -eo vi; ls",
        "ls; set -e; ls",
        "set -e; ls; set -u; ls",
        "builtin set -o posix; ls",
        "(set -e; ls; set -B; ls)",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "set -e; ls",
        "set -euo pipefail; cargo test",
        "set -o errexit -o nounset; ls",
        "set -x; set -o pipefail; ls",
        "set -euxo pipefail\ncargo build",
        "shopt -q extglob; ls",
        "set -- a b; ls",
        "shopt -s lastpipe",
        "ls; set -o posix",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn and_chains_keep_intermediate_directories() {
    let c = parse_commands("cd a && cd b; ls").unwrap();
    let expect: std::collections::BTreeSet<String> =
        ["", "a", "a/b"].iter().map(|s| s.to_string()).collect();
    assert_eq!(cwd_set(&c[2]), expect);
    // The right side of && still runs only after the left side succeeded.
    let c = parse_commands("cd a && cd b && ls").unwrap();
    assert_eq!(cwds(&c[2]), ["a/b"]);
    let c = parse_commands("cd a || ls").unwrap();
    assert_eq!(
        cwd_set(&c[1]),
        ["", "a"].iter().map(|s| s.to_string()).collect()
    );
}

#[test]
fn printf_length_modifiers_and_time_conversions_are_numeric() {
    for s in [
        "printf '%ld' \"$x\"",
        "printf '%lld' \"$x\"",
        "printf '%hd' \"$x\"",
        "printf '%hhu' $x",
        "printf '%jd' \"$x\"",
        "printf '%zx' \"$x\"",
        "printf '%qd' $x",
        "printf '%td' $x",
        "printf '%Lx' $x",
        "printf '%(%Y)T' \"$x\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "printf '%lf' \"$x\"",
        "printf '%s' \"$x\"",
        "printf '%(%Y)T' 0",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

// ---- checkpoint fix round 5 ----

#[test]
fn builtin_double_dash_is_unwrapped() {
    for s in [
        "builtin -- cd sub && cat x",
        "builtin builtin cd sub && cat x",
        "command builtin -- cd sub && cat x",
    ] {
        let c = parse_commands(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        let cat = c.iter().find(|c| c.argv[0] == "cat").unwrap();
        assert_eq!(cwds(cat), ["sub"], "{s}");
    }
    for s in [
        "builtin -- cd .g*/hooks; echo x",
        "builtin -- read PATH; ls",
        "builtin -- shift \"$x\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
}

#[test]
fn assigning_expansions_of_dangerous_names_are_unparseable() {
    for s in [
        "echo ${CDPATH:=.git}; cd hooks && echo pwn > pre-commit",
        "echo ${CDPATH=.git}",
        "echo \"${PATH:=/tmp/evil}\" > /dev/null; ls",
        "cat ${IFS:=x}/config",
        "echo ${LD_PRELOAD?x}",
        "echo ${GIT_DIR:?x}",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    assert!(parse_commands("echo ${FOO:=x} ${PATH:-x} ${HOME:+y} ${CDPATH} ${#PATH}").is_ok());
}

#[test]
fn line_continuation_inside_a_word_is_unparseable() {
    for s in [
        "c\\\nd .git && echo pwn > hooks/pre-commit",
        "ec\\\nho x",
        "cat .g\\\nit/config",
        "echo \"a\\\nb\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s:?}");
    }
    for s in ["cargo test \\\n  --all", "echo a \\\nb"] {
        assert!(parse_commands(s).is_ok(), "{s:?}");
    }
}

#[test]
fn redefining_sensitive_commands_makes_later_commands_unparseable() {
    for s in [
        "cd() { true; }; cd sub/x && echo pwn > ../../pwned",
        "function cd { true; }; cd sub/x",
        "builtin() { true; }; builtin cd sub/x",
        "command() { true; }; ls",
        "set() { :; }; ls",
        "printf() { :; }; ls",
        "[() { :; }; ls",
        "enable -n cd; cd sub/x && echo pwn > ../../pwned",
        "enable -n test; ls",
        "alias cd=true\ncd sub/x && echo pwn > ../../pwned",
        "alias ls=x eval=y; ls",
    ] {
        assert!(parse_commands(s).is_err(), "{s:?}");
    }
    for s in [
        "f() { ls; }; f",
        "alias ll='ls -l'; ls",
        "enable -n kill; ls",
        "cd() { true; }",
    ] {
        assert!(parse_commands(s).is_ok(), "{s:?}");
    }
}

#[test]
fn printf_nonstandard_length_modifiers_are_numeric() {
    for s in ["printf '%Zd' \"$x\"", "printf '%Id' \"$x\""] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
}

// --- line continuations that split a word (bash joins them, tree-sitter does not) ---

fn assert_split(src: &str) {
    let err = parse_commands(src).expect_err(&format!("must be unparseable: {src:?}"));
    assert!(
        err.0.contains("line continuation splits a word"),
        "wrong reason for {src:?}: {err}"
    );
}

#[test]
fn continuation_splitting_a_redirect_target_is_unparseable() {
    assert_split("echo pwn > .g\\\nit/hooks/pre-commit");
    assert_split("echo pwn >> .g\\\nit/hooks/pre-commit");
    assert_split("echo pwn 2> .g\\\nit/hooks/pre-commit");
    assert_split("echo pwn &> .g\\\nit/hooks/pre-commit");
    assert_split("{ echo pwn; } > .g\\\nit/hooks/pre-commit");
}

#[test]
fn continuation_splitting_an_exported_name_or_value_is_unparseable() {
    assert_split("export CDP\\\nATH=.g\\\nit; cd hooks && echo pwn > pre-commit");
    assert_split("declare CDP\\\nATH=x; cd y");
    assert_split("readonly CDP\\\nATH=x; cd y");
    assert_split("export L\\\nD_PRELOAD=x; cargo test");
}

#[test]
fn continuation_before_whitespace_still_parses() {
    let c = parse_commands("cargo test \\\n  --workspace").unwrap();
    assert_eq!(c[0].argv, ["cargo", "test", "--workspace"]);
    let c = parse_commands("docker run \\\n  -it x").unwrap();
    assert_eq!(c[0].argv, ["docker", "run", "-it", "x"]);
    assert!(parse_commands("cargo test \\\n\tfoo").is_ok());
}

#[test]
fn continuation_inside_single_quotes_still_parses() {
    let c = parse_commands("echo 'a\\\nb'").unwrap();
    assert_eq!(argv0s(&c), ["echo"]);
}

#[test]
fn continuation_inside_quoted_heredoc_body_still_parses() {
    let c = parse_commands("cat <<'EOF'\nfoo \\\nx\nEOF\n").unwrap();
    assert_eq!(argv0s(&c), ["cat"]);
}

#[test]
fn continuation_in_double_quotes_or_unquoted_heredoc_is_unparseable() {
    assert_split("echo \"a\\\nb\"");
    assert_split("cat <<EOF\nfoo\\\nx\nEOF\n");
}

#[test]
fn apostrophe_in_comment_does_not_hide_a_later_continuation() {
    assert_split("echo hi # it's\necho pwn > .g\\\nit/hooks/pre-commit");
}

#[test]
fn continuation_after_whitespace_or_operator_does_not_split_a_word() {
    let c = parse_commands("cargo test \\\n--workspace").unwrap();
    assert_eq!(c[0].argv, ["cargo", "test", "--workspace"]);
    let c = parse_commands("echo a\t\\\nb").unwrap();
    assert_eq!(c[0].argv, ["echo", "a", "b"]);
    let c = parse_commands("cargo build &&\\\ncargo test").unwrap();
    assert_eq!(argv0s(&c), ["cargo", "cargo"]);
    let c = parse_commands("cargo build ||\\\ncargo test").unwrap();
    assert_eq!(argv0s(&c), ["cargo", "cargo"]);
    let c = parse_commands("ls |\\\nhead").unwrap();
    assert_eq!(argv0s(&c), ["ls", "head"]);
    let c = parse_commands("ls;\\\nhead").unwrap();
    assert_eq!(argv0s(&c), ["ls", "head"]);
    let c = parse_commands("(\\\nls)").unwrap();
    assert_eq!(argv0s(&c), ["ls"]);
}

#[test]
fn continuation_after_an_escaped_or_quoted_lookalike_still_splits() {
    // The preceding byte is a space or `;`, but not whitespace/an operator to the shell.
    assert_split("echo a\\ \\\nb");
    assert_split("echo a\\;\\\nb");
    assert_split("echo \"a;\\\nb\"");
    assert_split("echo pwn > .g\\ \\\nit/hooks/pre-commit");
}

#[test]
fn continuation_that_would_merge_two_operators_is_unparseable() {
    for s in [
        "ls |\\\n| head",
        "ls &&\\\n& head",
        "ls;\\\n; head",
        "echo $(\\\n(1))",
        "echo a >\\\n> .git/x",
    ] {
        assert!(parse_commands(s).is_err(), "{s:?}");
    }
}

// --- bytes bash treats differently from the grammar: \r, \f, \v ---

#[test]
fn carriage_return_form_feed_and_vertical_tab_are_unparseable() {
    for s in [
        "ls \\\r\necho SECOND",
        "ls\recho SECOND",
        "ls\r\necho SECOND",
        "ls\x0cecho SECOND",
        "ls\x0becho SECOND",
        "echo hi # c\rmment",
        "echo \"a\rb\"",
    ] {
        assert!(parse_commands(s).is_err(), "{s:?}");
    }
}

#[test]
fn control_whitespace_inside_single_quotes_or_quoted_heredoc_still_parses() {
    assert!(parse_commands("echo 'a\rb\x0c\x0b'").is_ok());
    assert!(parse_commands("cat <<'EOF'\nfoo\r\nbar\x0c\nEOF\n").is_ok());
    assert!(parse_commands("cat <<EOF\nfoo\r\nEOF\n").is_err());
}

// ---- Task 30 re-review 3, N6: close operators and heredoc delimiters ----

#[test]
fn words_after_a_close_operator_or_heredoc_delimiter_are_unparseable() {
    // bash runs `cmd a` for each of these; the grammar files `a` under the redirect.
    for s in [
        "cmd >&- a",
        "cmd 2>&- a",
        "cmd <&- a",
        "cmd 3>&- a",
        "cmd <<EOF a b c\nx\nEOF\n",
        "cmd <<'EOF' a\nx\nEOF\n",
        "cmd <<-EOF a\nx\nEOF\n",
        "cmd <<\"EOF\" a\nx\nEOF\n",
        "cmd > out.txt <<'EOF' a\nx\nEOF\n",
        "cargo test <<EOF --config=build.rustc-wrapper=./evil.sh\nx\nEOF\n",
        "{ cmd; } >&- a",
        "( cmd ) <<EOF a\nx\nEOF\n",
        "f() { :; } >&- a",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    // Without a trailing word these stay analyzable.
    assert_eq!(parse_commands("cmd a >&-").unwrap()[0].argv, ["cmd", "a"]);
    assert!(
        parse_commands("cmd a 2>&-").unwrap()[0]
            .redirects
            .is_empty()
    );
    assert_eq!(
        parse_commands("cmd a <<EOF\nx\nEOF\n").unwrap()[0].argv,
        ["cmd", "a"]
    );
    assert_eq!(
        argv0s(&parse_commands("cmd <<EOF | next\nx\nEOF\n").unwrap()),
        ["cmd", "next"]
    );
    assert_eq!(
        argv0s(&parse_commands("cmd <<EOF && next\nx\nEOF\n").unwrap()),
        ["cmd", "next"]
    );
}

/// The structural invariant: a word that bash passes to a command is in some
/// command's argv or is a redirect target, or the input is unparseable. `ZZMARK`
/// stands wherever the grammar might file a word away from argv.
#[test]
fn every_word_reaches_argv_or_a_target_or_the_input_is_unparseable() {
    const M: &str = "ZZMARK";
    let shapes = [
        "cmd ZZMARK",
        "cmd > f ZZMARK",
        "cmd 2> f ZZMARK",
        "cmd >> f ZZMARK",
        "cmd &> f ZZMARK",
        "cmd >| f ZZMARK",
        "cmd < f ZZMARK",
        "cmd <> f ZZMARK",
        "cmd 2>&1 ZZMARK",
        "cmd >&2 ZZMARK",
        "cmd >& f ZZMARK",
        "cmd <& 3 ZZMARK",
        "cmd >&- ZZMARK",
        "cmd <&- ZZMARK",
        "cmd 2>&- ZZMARK",
        "cmd 9>&- ZZMARK",
        "cmd {fd}> f ZZMARK",
        "cmd {fd}>&- ZZMARK",
        "cmd ZZMARK >&-",
        "cmd > f 2>&- ZZMARK",
        "cmd >&- > f ZZMARK",
        "cmd >&- 'ZZMARK'",
        "cmd >&- \"ZZMARK\"",
        "cmd >&- $'ZZMARK'",
        "cmd >&- a\"ZZMARK\"",
        "cmd >&- ZZMARK=1",
        "cmd >&- --ZZMARK",
        "cmd >&- -ZZMARK",
        "cmd >&- ZZMARK > g",
        "cmd >&- ; ZZMARK",
        "cmd <<< hi ZZMARK",
        "cmd <<EOF ZZMARK\nx\nEOF\n",
        "cmd <<EOF a ZZMARK\nx\nEOF\n",
        "cmd <<EOF ZZMARK b\nx\nEOF\n",
        "cmd <<'EOF' ZZMARK\nx\nEOF\n",
        "cmd <<\"EOF\" ZZMARK\nx\nEOF\n",
        "cmd <<\\EOF ZZMARK\nx\nEOF\n",
        "cmd <<-EOF ZZMARK\nx\nEOF\n",
        "cmd 3<<EOF ZZMARK\nx\nEOF\n",
        "cmd <<EOF 'ZZMARK'\nx\nEOF\n",
        "cmd <<EOF \"ZZMARK\"\nx\nEOF\n",
        "cmd <<EOF ZZMARK > g\nx\nEOF\n",
        "cmd <<EOF > g ZZMARK\nx\nEOF\n",
        "cmd <<EOF 2> g ZZMARK\nx\nEOF\n",
        "cmd <<EOF >&- ZZMARK\nx\nEOF\n",
        "cmd <<EOF <<< hi ZZMARK\nx\nEOF\n",
        "cmd <<EOF | ZZMARK\nx\nEOF\n",
        "cmd <<EOF |& ZZMARK\nx\nEOF\n",
        "cmd <<EOF && ZZMARK\nx\nEOF\n",
        "cmd <<EOF || ZZMARK\nx\nEOF\n",
        "cmd <<EOF ; ZZMARK\nx\nEOF\n",
        "cmd <<EOF\nx\nEOF\nZZMARK\n",
        "cmd ZZMARK <<EOF\nx\nEOF\n",
        "cmd <<EOF <<EOF2 ZZMARK\nx\nEOF\ny\nEOF2\n",
        "cmd <<EOF ZZMARK <<EOF2\nx\nEOF\ny\nEOF2\n",
        "cmd > f\\\n ZZMARK",
        "cmd >&-\\\n ZZMARK",
        "cmd >&-\tZZMARK",
        "FOO=1 cmd >&- ZZMARK",
        "FOO=1 >&- cmd ZZMARK",
        "{ cmd; } > f ZZMARK",
        "{ cmd; } >&- ZZMARK",
        "{ cmd; } <<EOF ZZMARK\nx\nEOF\n",
        "( cmd ) >&- ZZMARK",
        "( cmd ) <<EOF ZZMARK\nx\nEOF\n",
        "f() { :; } >&- ZZMARK",
        "f() { :; } <<EOF ZZMARK\nx\nEOF\n",
        "if cmd; then :; fi >&- ZZMARK",
        "while cmd; do :; done <<EOF ZZMARK\nx\nEOF\n",
        "for x in a; do cmd; done >&- ZZMARK",
        "cmd | cmd2 >&- ZZMARK",
        "cmd && cmd2 >&- ZZMARK",
        "cmd $(cmd2 >&- ZZMARK)",
        "cmd <(cmd2 <<EOF ZZMARK\nx\nEOF\n)",
        "[[ -f x ]] >&- ZZMARK",
        "[ a > ZZMARK ]",
        "[ ZZMARK > Z ]",
        "cmd 0<&ZZMARK",
        "export A=1 >&- ZZMARK",
        "! cmd >&- ZZMARK",
    ];
    for s in shapes {
        let Ok(cmds) = parse_commands(s) else {
            continue;
        };
        let seen = cmds.iter().any(|c| {
            c.argv.iter().any(|w| w.contains(M)) || c.redirects.iter().any(|r| r.target == M)
        });
        assert!(seen, "{s:?} parsed as {cmds:?} but hides {M}");
    }
    // The invariant does not refuse the ordinary shapes.
    for s in [
        "cmd ZZMARK",
        "cmd ZZMARK >&-",
        "cmd ZZMARK <<EOF\nx\nEOF\n",
        "cmd <<EOF | ZZMARK\nx\nEOF\n",
        "cmd <<EOF && ZZMARK\nx\nEOF\n",
        "cmd <<EOF > g\nx\nEOF\n",
        "cmd <<< ZZMARK",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

// ---- Task 30 re-review 4, N9: `[ … ]` is the `[` command, so `>` inside is a redirect ----

#[test]
fn single_bracket_tests_holding_a_redirect_are_unparseable() {
    for s in [
        "[ a > Z ]",
        "[ a < Z ]",
        "[ a >= b ]",
        "[ 2>f ]",
        "[ a > .git/config ]",
        "[ -f x ] && [ a > Z ]",
        "if [ a > Z ]; then :; fi",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in ["[ -f x ]", "[ \"$a\" = \"$b\" ]", "[ a ]", "[ 1 -eq 1 ]"] {
        assert_eq!(parse_commands(s).unwrap()[0].argv, ["[["], "{s}");
    }
    // A redirect on the whole test is seen; `[[ … ]]` compares.
    let c = parse_commands("[ a ] > Z").unwrap();
    assert!(c[0].redirects.iter().any(|r| r.target == "Z" && r.write));
    assert_eq!(parse_commands("[[ a > b ]]").unwrap()[0].argv, ["[["]);
}

// ---- Task 30 re-review 4, N11: a descriptor glued to `<&` / `>&` is not a word ----

#[test]
fn leading_descriptor_of_a_redirect_is_not_an_argv_word() {
    assert_eq!(
        parse_commands("git push 0<&3").unwrap()[0].argv,
        ["git", "push"]
    );
    assert_eq!(parse_commands("cmd 0>&3").unwrap()[0].argv, ["cmd"]);
    assert_eq!(parse_commands("cmd 1>&2").unwrap()[0].argv, ["cmd"]);
    assert_eq!(parse_commands("cmd 10>&1").unwrap()[0].argv, ["cmd"]);
    let c = parse_commands("cmd 0>f").unwrap();
    assert_eq!(c[0].argv, ["cmd"]);
    assert!(c[0].redirects.iter().any(|r| r.target == "f" && r.write));
    // With a space the number is an argument, as in bash.
    assert_eq!(parse_commands("cmd 0 <&3").unwrap()[0].argv, ["cmd", "0"]);
    assert_eq!(parse_commands("cmd 0").unwrap()[0].argv, ["cmd", "0"]);
}

// ---- quality Q1: arithmetic literals, fail-closed arms, printf -v and option parsing ----

#[test]
fn arithmetic_operands_must_be_literals_that_cannot_run_code() {
    for s in [
        "for ((i=0;i<$(id);i++)); do :; done",
        "for ((i=$(id);;)); do :; done",
        "echo ${a[$(id)]}",
        "[[ $(id) -eq 1 ]]",
        "[[ 1 -ne $(id) ]]",
        "echo $((16#$(id)))",
        "echo $((2#$x))",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "for ((;;)); do break; done",
        "echo $((0x1f + 16#ff))",
        "echo $((2#101 * 8#17))",
        "echo ${a[3]}",
        "[[ 1 -le 2 ]]",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}

#[test]
fn hidden_writes_and_unknown_constructs_fail_closed() {
    // A write with no command, a word glued across a continuation.
    for s in ["> /etc/x", ">> log", "ec\\\nho hi", "rm -r\\\nf /"] {
        assert!(parse_commands(s).is_err(), "{s:?}");
    }
    // A `select` loop's body is analyzed like any other.
    let c = parse_commands("select x in a b; do rm $x; done").unwrap();
    assert_eq!(argv0s(&c), ["rm"]);
    let c = parse_commands("echo hi >file").unwrap();
    assert_eq!(
        c[0].redirects,
        vec![Redirect {
            target: "file".into(),
            write: true
        }]
    );
    // A continuation between words is only whitespace.
    let c = parse_commands("echo a \\\nb").unwrap();
    assert_eq!(c[0].argv, ["echo", "a", "b"]);
}

#[test]
fn printf_v_names_and_option_operands_are_judged_as_written() {
    // `printf -v` names a variable: only a static name may pass.
    for s in [
        "printf -v \"$name\" '%d' \"$x\"",
        "printf -v 'a[$(id)]' %s x",
        // `-v` takes the next word as the name, so `--` is the name here (as in bash).
        "printf -v -- foo %s \"$x\"",
        "echo ${a[1}",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    let c = parse_commands("printf -v foo %s x").unwrap();
    assert_eq!(c[0].argv, ["printf", "-v", "foo", "%s", "x"]);
    // After `--` a word that looks like an option is an operand, kept literally.
    let c = parse_commands("rm -- -rf").unwrap();
    assert_eq!(c[0].argv, ["rm", "--", "-rf"]);
    // Backslash escapes are removed as bash removes them.
    let c = parse_commands("echo \\<x \\$HOME a\\ b").unwrap();
    assert_eq!(c[0].argv, ["echo", "<x", "$HOME", "a b"]);
}

#[test]
fn interpreter_scripts_that_climb_out_or_read_stdin_are_unparseable() {
    for s in [
        "python ../evil.py",
        "python3 a/../../evil.py",
        "node ./../x.js",
        "python --",
        "python -- ../evil.py",
        "python -",
        "sh -s",
        "bash -s arg",
    ] {
        assert!(parse_commands(s).is_err(), "{s}");
    }
    for s in [
        "python a/../ok.py",
        "python ./x.py",
        "python -- x.py",
        "sh ./x.sh",
    ] {
        assert!(parse_commands(s).is_ok(), "{s}");
    }
}
