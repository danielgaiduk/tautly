use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tautly_core::auth::api_key_with;
use tautly_core::config::config_home_from;
use tautly_core::{
    AuthError, CliOverrides, Config, ConfigError, ProviderEntry, SecretStore, config_home,
};
use tautly_provider::{Catalog, ModelId};
use tautly_sandbox::Mode;
use tempfile::TempDir;

/// A user file, a project root and helpers to write their config files.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            dir: TempDir::new().unwrap(),
        }
    }

    fn user_file(&self) -> PathBuf {
        self.dir.path().join("user").join("config.toml")
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn user(&self, toml: &str) {
        let p = self.user_file();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, toml).unwrap();
    }

    fn project(&self, toml: &str) {
        let p = self.root().join(".tautly").join("config.toml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, toml).unwrap();
    }

    fn load(&self, cli: CliOverrides) -> Result<Config, ConfigError> {
        fs::create_dir_all(self.root()).unwrap();
        Config::load_from(Some(&self.user_file()), &self.root(), cli)
    }
}

#[test]
fn project_overrides_user_and_cli_overrides_project() {
    let f = Fixture::new();
    f.user(
        r#"
model = "openai/gpt-5"
max_turns = 50
turn_timeout_s = 600
approve_memory = true
[permissions]
mode = "auto-edit"
"#,
    );
    f.project(
        r#"
model = "google/gemini-3-pro"
max_turns = 30
[permissions]
mode = "ask"
"#,
    );
    let cli = CliOverrides {
        model: Some(ModelId::from("anthropic/claude-opus-5-5")),
        mode: Some(Mode::Yolo),
        approve_memory: None,
    };
    let cfg = f.load(cli).unwrap();
    assert_eq!(cfg.model.as_str(), "anthropic/claude-opus-5-5", "cli wins");
    assert_eq!(cfg.permissions.mode, Mode::Yolo, "cli wins over project");
    assert_eq!(cfg.max_turns, 30, "project wins over user");
    assert_eq!(cfg.turn_timeout_s, Some(600), "user-only key survives");
    assert!(cfg.approve_memory, "user-only key survives");

    // Without CLI flags the project layer is what is left on top of the user layer.
    let cfg = f.load(CliOverrides::default()).unwrap();
    assert_eq!(cfg.model.as_str(), "google/gemini-3-pro");
    assert_eq!(cfg.permissions.mode, Mode::Ask);
}

#[test]
fn tables_merge_deeply() {
    let f = Fixture::new();
    f.user(
        r#"
[providers.anthropic]
base_url = "https://proxy.example.com/"
[budget.session]
max_usd = 5.0
"#,
    );
    f.project(
        r#"
[providers.anthropic]
api_key_env = "WORK_ANTHROPIC_KEY"
[budget.session]
max_tokens = 100000
[budget.turn]
max_usd = 1.0
"#,
    );
    let cfg = f.load(CliOverrides::default()).unwrap();
    let a = cfg.providers.anthropic.unwrap();
    assert_eq!(a.base_url.unwrap().as_str(), "https://proxy.example.com/");
    assert_eq!(a.api_key_env.as_deref(), Some("WORK_ANTHROPIC_KEY"));
    assert_eq!(cfg.budget.session.max_usd, Some(5.0));
    assert_eq!(cfg.budget.session.max_tokens, Some(100_000));
    assert_eq!(cfg.budget.turn.max_usd, Some(1.0));
    assert_eq!(cfg.budget.turn.max_tokens, None);
}

#[test]
fn arrays_replace_not_concat() {
    let f = Fixture::new();
    f.user(
        r#"
pins = ["user pin 1", "user pin 2"]
[permissions]
allow = ["bash(cargo test*)", "bash(ls*)"]
deny = ["bash(rm -rf*)"]
[[providers.compat]]
name = "ollama"
base_url = "http://localhost:11434/v1"
[[providers.compat]]
name = "groq"
base_url = "https://api.groq.com/openai/v1"
"#,
    );
    f.project(
        r#"
pins = ["project pin"]
[permissions]
allow = ["bash(make*)"]
[[providers.compat]]
name = "vllm"
base_url = "http://localhost:8000/v1"
"#,
    );
    let cfg = f.load(CliOverrides::default()).unwrap();
    assert_eq!(cfg.pins, ["project pin"]);
    assert_eq!(cfg.permissions.allow, ["bash(make*)"]);
    assert_eq!(
        cfg.permissions.deny,
        ["bash(rm -rf*)"],
        "untouched array stays"
    );
    let names: Vec<_> = cfg.providers.compat.iter().map(|c| &*c.name).collect();
    assert_eq!(names, ["vllm"]);
}

#[test]
fn defaults_match_spec() {
    let f = Fixture::new();
    let cfg = f.load(CliOverrides::default()).unwrap();
    assert_eq!(cfg.compaction_threshold, Some(0.8));
    assert_eq!(cfg.max_turns, 100);
    assert!(!cfg.telemetry.enabled);
    assert_eq!(cfg.permissions.mode, Mode::Ask);
    assert!(cfg.permissions.allow.is_empty() && cfg.permissions.deny.is_empty());
    assert!(!cfg.approve_memory);
    assert_eq!(cfg.turn_timeout_s, None);
    assert!(cfg.pins.is_empty());
    assert!(cfg.catalog_overrides.is_none());
    assert_eq!(cfg.routing.side_model, None);
    assert_eq!(cfg.budget, Default::default());
    assert!(
        Catalog::builtin().contains(&cfg.model),
        "default model is in the catalog"
    );

    // The no-file result and `Config::default()` agree.
    let d = Config::default();
    assert_eq!(d.model, cfg.model);
    assert_eq!(d.compaction_threshold, cfg.compaction_threshold);
    assert_eq!(d.max_turns, cfg.max_turns);
}

#[test]
fn permissions_without_mode_default_to_ask() {
    let f = Fixture::new();
    f.project("[permissions]\nallow = [\"bash(ls*)\"]\n");
    let cfg = f.load(CliOverrides::default()).unwrap();
    assert_eq!(cfg.permissions.mode, Mode::Ask);
    assert_eq!(cfg.permissions.allow, ["bash(ls*)"]);
}

#[test]
fn zero_threshold_disables_compaction() {
    let f = Fixture::new();
    f.user("compaction_threshold = 0\n");
    assert_eq!(
        f.load(CliOverrides::default())
            .unwrap()
            .compaction_threshold,
        None
    );
    f.project("compaction_threshold = 0.6\n");
    assert_eq!(
        f.load(CliOverrides::default())
            .unwrap()
            .compaction_threshold,
        Some(0.6)
    );
}

#[test]
fn catalog_overrides_parse_into_a_catalog() {
    let f = Fixture::new();
    f.user(
        r#"
[catalog_overrides.models."compat:local/llama"]
context_window = 32000
max_output = 4096
parallel_tools = false
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#,
    );
    let cfg = f.load(CliOverrides::default()).unwrap();
    let id = ModelId::from("compat:local/llama");
    let mut catalog = Catalog::builtin();
    catalog.merge(cfg.catalog_overrides.unwrap());
    assert!(catalog.contains(&id));
    assert_eq!(catalog.get(&id).caps.context_window, 32000);
}

#[test]
fn bad_values_are_rejected_with_their_file() {
    let f = Fixture::new();
    f.user("max_turns = \"many\"\n");
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(
        matches!(&err, ConfigError::Invalid { origin: Some(p), .. } if *p == f.user_file()),
        "{err}"
    );
    assert!(err.to_string().contains("config.toml"), "{err}");

    let f = Fixture::new();
    f.project("unknown_key = 1\n");
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(err.to_string().contains("unknown_key"), "{err}");
    assert!(err.to_string().contains(".tautly"), "{err}");

    let f = Fixture::new();
    f.project("model = [unterminated\n");
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
}

#[test]
fn out_of_range_values_are_rejected() {
    for bad in [
        "compaction_threshold = 1.5\n",
        "compaction_threshold = -0.2\n",
        "max_turns = 0\n",
        "turn_timeout_s = 0\n",
        "[budget.session]\nmax_usd = -1.0\n",
        "[providers.openai]\nbase_url = \"ftp://example.com/\"\n",
        "[[providers.compat]]\nname = \"a/b\"\nbase_url = \"http://x/\"\n",
        "[[providers.compat]]\nname = \"a\"\nbase_url = \"http://x/\"\n[[providers.compat]]\nname = \"a\"\nbase_url = \"http://y/\"\n",
        "pins = [\"\"]\n",
    ] {
        let f = Fixture::new();
        f.project(bad);
        let err = f.load(CliOverrides::default()).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "{bad:?} gave {err}"
        );
    }
}

#[test]
fn project_config_that_is_not_a_plain_file_is_refused() {
    let f = Fixture::new();
    let dir = f.root().join(".tautly");
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir(dir.join("config.toml")).unwrap();
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Unsafe { .. }), "{err}");

    let f = Fixture::new();
    f.project(&format!("pins = [\"{}\"]\n", "x".repeat(300 * 1024)));
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Unsafe { .. }), "{err}");
}

#[test]
fn project_config_symlink_is_refused_but_user_symlink_is_followed() {
    let f = Fixture::new();
    let real = f.dir.path().join("real.toml");
    fs::write(&real, "max_turns = 7\n").unwrap();

    let dir = f.root().join(".tautly");
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("config.toml")).unwrap();
    let err = f.load(CliOverrides::default()).unwrap_err();
    assert!(matches!(err, ConfigError::Unsafe { .. }), "{err}");

    let f2 = Fixture::new();
    let real = f2.dir.path().join("real.toml");
    fs::write(&real, "max_turns = 7\n").unwrap();
    fs::create_dir_all(f2.user_file().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, f2.user_file()).unwrap();
    assert_eq!(f2.load(CliOverrides::default()).unwrap().max_turns, 7);
}

#[test]
fn config_home_resolution() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        let map: HashMap<&str, OsString> =
            pairs.iter().map(|(k, v)| (*k, OsString::from(v))).collect();
        move |name: &str| map.get(name).cloned()
    };
    assert_eq!(
        config_home_from(env(&[
            ("TAUTLY_HOME", "/t"),
            ("XDG_CONFIG_HOME", "/x"),
            ("HOME", "/h")
        ])),
        Path::new("/t")
    );
    assert_eq!(
        config_home_from(env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])),
        Path::new("/x/tautly")
    );
    assert_eq!(
        config_home_from(env(&[("HOME", "/h")])),
        Path::new("/h/.config/tautly")
    );
    // Empty variables count as unset.
    assert_eq!(
        config_home_from(env(&[
            ("TAUTLY_HOME", ""),
            ("XDG_CONFIG_HOME", ""),
            ("HOME", "/h")
        ])),
        Path::new("/h/.config/tautly")
    );
    // The real function agrees with itself and ends in `tautly` unless overridden.
    let home = config_home();
    assert!(home.ends_with("tautly") || std::env::var_os("TAUTLY_HOME").is_some());
}

// ---- auth ----

#[derive(Default)]
struct FakeStore {
    secrets: HashMap<(String, String), String>,
    fail: bool,
    calls: Mutex<Vec<(String, String)>>,
}

impl FakeStore {
    fn with(service: &str, account: &str, secret: &str) -> Self {
        let mut s = FakeStore::default();
        s.secrets
            .insert((service.into(), account.into()), secret.into());
        s
    }
}

impl SecretStore for FakeStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError> {
        self.calls
            .lock()
            .unwrap()
            .push((service.into(), account.into()));
        if self.fail {
            return Err(AuthError::Keychain("locked".into()));
        }
        Ok(self.secrets.get(&(service.into(), account.into())).cloned())
    }
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn env_key_wins_over_keychain() {
    let store = FakeStore::with("tautly", "anthropic", "from-keychain");
    let key = api_key_with(
        "anthropic",
        &ProviderEntry::default(),
        &env_of(&[("ANTHROPIC_API_KEY", "from-env")]),
        &store,
    )
    .unwrap();
    assert_eq!(key.as_deref(), Some("from-env"));
    assert!(
        store.calls.lock().unwrap().is_empty(),
        "keychain not consulted when the env var is set"
    );
}

#[test]
fn keychain_is_used_when_env_is_unset_or_blank() {
    let store = FakeStore::with("tautly", "openai", "from-keychain");
    for env in [
        env_of(&[]),
        env_of(&[("OPENAI_API_KEY", "")]),
        env_of(&[("OPENAI_API_KEY", "  \n")]),
    ] {
        let key = api_key_with("openai", &ProviderEntry::default(), &env, &store).unwrap();
        assert_eq!(key.as_deref(), Some("from-keychain"));
    }
    assert_eq!(
        store.calls.lock().unwrap()[0],
        ("tautly".to_string(), "openai".to_string())
    );
}

#[test]
fn default_env_names_per_provider_and_configured_name_replaces_them() {
    let none = FakeStore::default();
    let get = |provider: &str, entry: &ProviderEntry, env: &[(&str, &str)]| {
        api_key_with(provider, entry, &env_of(env), &none).unwrap()
    };
    let d = ProviderEntry::default();
    assert_eq!(
        get("anthropic", &d, &[("ANTHROPIC_API_KEY", "a")]).as_deref(),
        Some("a")
    );
    assert_eq!(
        get("openai", &d, &[("OPENAI_API_KEY", "o")]).as_deref(),
        Some("o")
    );
    assert_eq!(
        get("google", &d, &[("GEMINI_API_KEY", "g")]).as_deref(),
        Some("g")
    );
    // A provider with no default name has no env source.
    assert_eq!(get("compat:ollama", &d, &[("OPENAI_API_KEY", "o")]), None);

    let custom = ProviderEntry {
        api_key_env: Some("MY_KEY".into()),
        ..Default::default()
    };
    assert_eq!(
        get(
            "anthropic",
            &custom,
            &[("MY_KEY", "m"), ("ANTHROPIC_API_KEY", "a")]
        )
        .as_deref(),
        Some("m")
    );
    assert_eq!(
        get("anthropic", &custom, &[("ANTHROPIC_API_KEY", "a")]),
        None
    );
    assert_eq!(
        get("compat:ollama", &custom, &[("MY_KEY", " k \n")]).as_deref(),
        Some("k")
    );
}

#[test]
fn no_key_anywhere_is_none_and_keychain_errors_propagate() {
    let store = FakeStore::default();
    assert_eq!(
        api_key_with("google", &ProviderEntry::default(), &env_of(&[]), &store).unwrap(),
        None
    );
    let failing = FakeStore {
        fail: true,
        ..Default::default()
    };
    let err =
        api_key_with("google", &ProviderEntry::default(), &env_of(&[]), &failing).unwrap_err();
    assert!(matches!(err, AuthError::Keychain(_)));
}

#[test]
fn non_unicode_env_value_is_an_error_not_a_silent_skip() {
    use std::os::unix::ffi::OsStringExt;
    let env =
        |name: &str| (name == "ANTHROPIC_API_KEY").then(|| OsString::from_vec(vec![0xff, 0xfe]));
    let err = api_key_with(
        "anthropic",
        &ProviderEntry::default(),
        &env,
        &FakeStore::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, AuthError::EnvNotUnicode { name } if name == "ANTHROPIC_API_KEY"),
        "{err}"
    );
    assert!(
        !err.to_string().contains('\u{fffd}'),
        "the value is never echoed"
    );
}
