//! Review S2-I1 (probe `pyc.sh`): a repository can ship a hash-based ("unchecked") `.pyc`
//! in a `__pycache__`, and `python -m pkg` runs it instead of the keyed source. The
//! engine sets `PYTHONPYCACHEPREFIX` to a private directory for every stdio server
//! (`tautly-core/tests/mcp.rs::stdio_servers_get_a_private_pycache_prefix`, `env.rs`);
//! this shows that the prefix is what stops the hostile bytecode from running. Since
//! review S3-I1 the key also covers such bytecode (`approvals.rs::hash_based_bytecode_is_keyed`),
//! for command lines that drop the prefix. Skipped without a `python3`.

use std::path::Path;
use std::process::Command;

fn python(cwd: &Path, args: &[&str], prefix: Option<&Path>) -> Option<String> {
    let mut cmd = Command::new("python3");
    cmd.current_dir(cwd)
        .args(args)
        .env_remove("PYTHONPYCACHEPREFIX");
    cmd.env_remove("PYTHONDONTWRITEBYTECODE");
    if let Some(p) = prefix {
        cmd.env("PYTHONPYCACHEPREFIX", p);
    }
    let out = cmd.output().ok()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn private_pycache_prefix_keeps_a_hostile_pyc_from_running() {
    let top = tempfile::tempdir().unwrap();
    let repo = top.path().join("repo");
    let pkg = repo.join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("__init__.py"), "").unwrap();
    std::fs::write(pkg.join("__main__.py"), "print('BENIGN')\n").unwrap();
    if python(&repo, &["-c", "pass"], None).is_none() {
        eprintln!("no python3: skipped");
        return;
    }
    // The hostile revision: unchanged source, plus unchecked-hash bytecode compiled from
    // other code, where the import system looks for it.
    let hostile = top.path().join("hostile.py");
    std::fs::write(&hostile, "print('HOSTILE CODE RAN')\n").unwrap();
    let compile = format!(
        "import py_compile, sys\n\
         tag = sys.implementation.cache_tag\n\
         py_compile.compile({src:?}, cfile='pkg/__pycache__/__main__.' + tag + '.pyc',\n\
         invalidation_mode=py_compile.PycInvalidationMode.UNCHECKED_HASH, doraise=True)\n",
        src = hostile.to_str().unwrap()
    );
    python(&repo, &["-c", &compile], None).unwrap();

    // With the engine's private prefix: the repository's `__pycache__` is never read.
    let private = top.path().join("private-cache");
    let ran = python(&repo, &["-m", "pkg"], Some(&private)).unwrap();
    assert_eq!(ran, "BENIGN");
    // Without any prefix (the default of python.org, Homebrew, uv and Linux Pythons;
    // Apple's command-line-tools Python has a built-in one, so the default is restored by
    // hand here): the bytecode runs instead of the source, which the prefix prevents.
    let unprefixed = "import sys, runpy\n\
                      sys.pycache_prefix = None\n\
                      runpy.run_module('pkg', run_name='__main__', alter_sys=True)\n";
    let ran = python(&repo, &["-c", unprefixed], None).unwrap();
    assert_eq!(ran, "HOSTILE CODE RAN");
}
