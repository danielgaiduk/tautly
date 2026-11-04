//! Zero-config first run (spec 7.11.2): provider detection, default models, the built-in
//! compat entries and the reserved `[telemetry]` table. Every test fakes the environment,
//! the keychain and Ollama: nothing here reads the real keychain or the network (the probe
//! tests talk to a local mock server).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tautly_core::config::{OLLAMA_URL, OPENROUTER_KEY_ENV, OPENROUTER_URL, TELEMETRY_WARNING};
use tautly_core::detect::{
    Detected, HttpOllamaProbe, KeySource, ModelChoice, OLLAMA_PROBE_TIMEOUT, OllamaProbe,
    default_model, detect, ollama_model, resolve_model, wants_detection,
};
use tautly_core::{AuthError, CliOverrides, Config, LoadedConfig, SecretStore, provider_for_with};
use tautly_provider::{Catalog, ModelId};
use tempfile::TempDir;

// ---- fakes ----

#[derive(Default)]
struct FakeStore {
    secrets: HashMap<String, String>,
    unavailable: bool,
    calls: Mutex<Vec<String>>,
}

impl FakeStore {
    fn with(accounts: &[&str]) -> Self {
        FakeStore {
            secrets: accounts
                .iter()
                .map(|a| (a.to_string(), format!("key-{a}")))
                .collect(),
            ..FakeStore::default()
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SecretStore for FakeStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError> {
        assert_eq!(service, "tautly");
        self.calls.lock().unwrap().push(account.to_string());
        if self.unavailable {
            return Err(AuthError::KeychainUnavailable("no Secret Service".into()));
        }
        Ok(self.secrets.get(account).cloned())
    }
}

/// A store that must not be asked.
struct NoStore;

impl SecretStore for NoStore {
    fn get(&self, _: &str, account: &str) -> Result<Option<String>, AuthError> {
        panic!("the keychain was read ({account}) although detection should not run")
    }
}

#[derive(Default)]
struct FakeProbe {
    models: Option<Vec<String>>,
    calls: Mutex<u32>,
}

impl FakeProbe {
    fn with(models: &[&str]) -> Self {
        FakeProbe {
            models: Some(models.iter().map(|m| m.to_string()).collect()),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl OllamaProbe for FakeProbe {
    fn installed_models(&self, timeout: Duration) -> Option<Vec<String>> {
        assert_eq!(timeout, OLLAMA_PROBE_TIMEOUT);
        *self.calls.lock().unwrap() += 1;
        self.models.clone()
    }
}

/// A probe that must not be asked.
struct NoProbe;

impl OllamaProbe for NoProbe {
    fn installed_models(&self, _: Duration) -> Option<Vec<String>> {
        panic!("Ollama was probed although detection should not run")
    }
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect();
    move |name| map.get(name).cloned()
}

fn id(s: &str) -> ModelId {
    ModelId::from(s)
}

// ---- detection order ----

#[test]
fn detect_order_env_then_keychain_then_openrouter_then_ollama() {
    struct Case {
        name: &'static str,
        env: &'static [(&'static str, &'static str)],
        keychain: &'static [&'static str],
        ollama: Option<&'static [&'static str]>,
        want: Option<(&'static str, &'static str, KeySource)>,
        /// The keychain accounts read, in order.
        reads: &'static [&'static str],
        probed: bool,
    }
    let all_keychain: &[&str] = &["anthropic", "openai", "google", "compat:openrouter"];
    let natives: &[&str] = &["anthropic", "openai", "google"];
    let cases = [
        Case {
            name: "every env key: anthropic first, nothing else asked",
            env: &[
                ("ANTHROPIC_API_KEY", "a"),
                ("OPENAI_API_KEY", "o"),
                ("GEMINI_API_KEY", "g"),
                ("OPENROUTER_API_KEY", "r"),
            ],
            keychain: all_keychain,
            ollama: Some(&["llama3.1:8b"]),
            want: Some((
                "anthropic/claude-sonnet-5-5",
                "anthropic",
                KeySource::Env("ANTHROPIC_API_KEY".into()),
            )),
            reads: &[],
            probed: false,
        },
        Case {
            name: "openai env before gemini env",
            env: &[("GEMINI_API_KEY", "g"), ("OPENAI_API_KEY", "o")],
            keychain: &[],
            ollama: None,
            want: Some((
                "openai/gpt-5",
                "openai",
                KeySource::Env("OPENAI_API_KEY".into()),
            )),
            reads: &[],
            probed: false,
        },
        Case {
            name: "any env key before any keychain entry",
            env: &[("GEMINI_API_KEY", "g")],
            keychain: all_keychain,
            ollama: None,
            want: Some((
                "google/gemini-3-pro",
                "google",
                KeySource::Env("GEMINI_API_KEY".into()),
            )),
            reads: &[],
            probed: false,
        },
        Case {
            name: "a blank env key counts as unset",
            env: &[("ANTHROPIC_API_KEY", "  \n")],
            keychain: &["openai"],
            ollama: None,
            want: Some((
                "openai/gpt-5",
                "openai",
                KeySource::Keychain("openai".into()),
            )),
            reads: &["anthropic", "openai"],
            probed: false,
        },
        Case {
            name: "native keychain entries before OpenRouter",
            env: &[("OPENROUTER_API_KEY", "r")],
            keychain: &["google", "compat:openrouter"],
            ollama: Some(&["llama3.1:8b"]),
            want: Some((
                "google/gemini-3-pro",
                "google",
                KeySource::Keychain("google".into()),
            )),
            reads: natives,
            probed: false,
        },
        Case {
            name: "OpenRouter env before its keychain entry",
            env: &[("OPENROUTER_API_KEY", "r")],
            keychain: &["compat:openrouter"],
            ollama: Some(&["llama3.1:8b"]),
            want: Some((
                "compat:openrouter/openai/gpt-5",
                "compat:openrouter",
                KeySource::Env("OPENROUTER_API_KEY".into()),
            )),
            reads: natives,
            probed: false,
        },
        Case {
            name: "OpenRouter keychain entry",
            env: &[],
            keychain: &["compat:openrouter"],
            ollama: Some(&["llama3.1:8b"]),
            want: Some((
                "compat:openrouter/openai/gpt-5",
                "compat:openrouter",
                KeySource::Keychain("compat:openrouter".into()),
            )),
            reads: all_keychain,
            probed: false,
        },
        Case {
            name: "Ollama last",
            env: &[],
            keychain: &[],
            ollama: Some(&["llama3.1:8b"]),
            want: Some((
                "compat:ollama/llama3.1:8b",
                "compat:ollama",
                KeySource::Ollama,
            )),
            reads: all_keychain,
            probed: true,
        },
        Case {
            name: "nothing",
            env: &[],
            keychain: &[],
            ollama: None,
            want: None,
            reads: all_keychain,
            probed: true,
        },
    ];
    let catalog = Catalog::builtin();
    for case in cases {
        let store = FakeStore::with(case.keychain);
        let probe = match case.ollama {
            Some(models) => FakeProbe::with(models),
            None => FakeProbe::default(),
        };
        let got = detect(&env_of(case.env), &store, &probe, &catalog).unwrap();
        let want = case.want.map(|(model, provider, source)| Detected {
            model: id(model),
            provider: provider.to_string(),
            source,
            warning: None,
        });
        assert_eq!(got, want, "{}", case.name);
        assert_eq!(store.calls(), case.reads, "{}: keychain reads", case.name);
        assert_eq!(probe.calls() > 0, case.probed, "{}: probed", case.name);
    }
}

#[test]
fn an_unavailable_keychain_is_skipped_not_an_error() {
    let store = FakeStore {
        unavailable: true,
        ..FakeStore::default()
    };
    let probe = FakeProbe::with(&["llama3.1:8b"]);
    let got = detect(&env_of(&[]), &store, &probe, &Catalog::builtin())
        .unwrap()
        .unwrap();
    assert_eq!(got.source, KeySource::Ollama);
    // Asked once: a keychain that cannot be asked is not asked again for each account.
    assert_eq!(store.calls(), ["anthropic"]);
}

#[test]
fn a_non_unicode_env_key_is_an_error_naming_the_variable_only() {
    let env = |name: &str| (name == "OPENAI_API_KEY").then(|| OsString::from_vec(vec![0xff]));
    let err = detect(&env, &NoStore, &NoProbe, &Catalog::builtin()).unwrap_err();
    assert!(err.to_string().contains("OPENAI_API_KEY"), "{err}");
}

#[test]
fn startup_line_names_the_model_and_where_the_key_came_from() {
    let d = Detected {
        model: id("anthropic/claude-sonnet-5-5"),
        provider: "anthropic".into(),
        source: KeySource::Env("ANTHROPIC_API_KEY".into()),
        warning: None,
    };
    assert_eq!(
        d.startup_line(),
        "using anthropic/claude-sonnet-5-5 (from ANTHROPIC_API_KEY) · /model to switch · \
         tautly doctor to check"
    );
    let k = Detected {
        source: KeySource::Keychain("openai".into()),
        ..d.clone()
    };
    assert!(
        k.startup_line().contains("(from the keychain)"),
        "{}",
        k.startup_line()
    );
    let o = Detected {
        source: KeySource::Ollama,
        ..d
    };
    assert!(
        o.startup_line().contains("(from Ollama)"),
        "{}",
        o.startup_line()
    );
}

// ---- default models ----

fn entry(id: &str, tier: Option<&str>, window: u32, price: Option<(f64, f64)>) -> String {
    let mut s = format!(
        "[models.\"{id}\"]\ncontext_window = {window}\nmax_output = 8000\nparallel_tools = true\n\
         reasoning = \"none\"\ncache = \"none\"\nimages = false\nedit_format = \"str_replace\"\n"
    );
    if let Some(t) = tier {
        s.push_str(&format!("tier = \"{t}\"\n"));
    }
    if let Some((input, output)) = price {
        s.push_str(&format!(
            "[models.\"{id}\".price]\ninput = {input}\noutput = {output}\ncache_read = 0.0\n\
             cache_write = 0.0\n"
        ));
    }
    s
}

#[test]
fn default_model_is_cheapest_non_small_with_128k_window() {
    let base = [
        entry("anthropic/large", Some("large"), 200_000, Some((3.0, 15.0))),
        entry("anthropic/larger", None, 200_000, Some((5.0, 25.0))),
        entry("anthropic/small", Some("small"), 200_000, Some((1.0, 5.0))),
        entry("anthropic/narrow", None, 100_000, Some((0.5, 1.0))),
        entry("anthropic/unpriced", None, 200_000, None),
        entry("openai/other", None, 400_000, Some((0.1, 0.2))),
        entry(
            "compat:openrouter/anthropic/x",
            None,
            200_000,
            Some((0.2, 0.2)),
        ),
        entry("compat:ollama/cheap", None, 200_000, Some((0.01, 0.01))),
    ]
    .join("\n");
    let catalog = Catalog::from_toml(&base).unwrap();
    assert_eq!(
        default_model("anthropic", &catalog),
        Some(id("anthropic/large"))
    );
    assert_eq!(
        default_model("compat:openrouter", &catalog),
        Some(id("compat:openrouter/anthropic/x"))
    );
    assert_eq!(default_model("google", &catalog), None);

    // Equal input price: the lower output price; then the id.
    let ties = [
        base.clone(),
        entry("anthropic/tie-b", None, 128_000, Some((3.0, 12.0))),
        entry("anthropic/tie-a", None, 128_000, Some((3.0, 12.0))),
    ]
    .join("\n");
    let catalog = Catalog::from_toml(&ties).unwrap();
    assert_eq!(
        default_model("anthropic", &catalog),
        Some(id("anthropic/tie-a"))
    );

    // The built-in catalog: each provider's main model, and OpenRouter's cheapest.
    let builtin = Catalog::builtin();
    for (provider, want) in [
        ("anthropic", "anthropic/claude-sonnet-5-5"),
        ("openai", "openai/gpt-5"),
        ("google", "google/gemini-3-pro"),
        ("compat:openrouter", "compat:openrouter/openai/gpt-5"),
    ] {
        assert_eq!(
            default_model(provider, &builtin),
            Some(id(want)),
            "{provider}"
        );
    }
    // Ollama models are unpriced: never a default this way.
    assert_eq!(default_model("compat:ollama", &builtin), None);
}

#[test]
fn ollama_prefers_catalog_model_else_alphabetical_with_warning() {
    let catalog = Catalog::from_toml(
        &[
            entry("compat:ollama/qwen3-coder:30b", None, 128_000, None),
            entry("compat:ollama/llama3.1:8b", None, 128_000, None),
        ]
        .join("\n"),
    )
    .unwrap();
    let installed = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

    // Catalog order (by id), not the order Ollama lists them in.
    let (model, warning) = ollama_model(
        &installed(&["zeta:1b", "qwen3-coder:30b", "llama3.1:8b"]),
        &catalog,
    );
    assert_eq!(model, id("compat:ollama/llama3.1:8b"));
    assert_eq!(warning, None);

    let (model, warning) = ollama_model(&installed(&["zeta:1b", "alpha:2b"]), &catalog);
    assert_eq!(model, id("compat:ollama/alpha:2b"));
    let warning = warning.expect("an uncatalogued model warns");
    assert!(warning.contains("alpha:2b"), "{warning}");

    // Through detection, the warning comes along.
    let probe = FakeProbe::with(&["zeta:1b", "alpha:2b"]);
    let got = detect(&env_of(&[]), &FakeStore::default(), &probe, &catalog)
        .unwrap()
        .unwrap();
    assert_eq!(got.model, id("compat:ollama/alpha:2b"));
    assert!(got.warning.is_some());

    // The built-in catalog knows common Ollama models, unpriced.
    let builtin = Catalog::builtin();
    let known: Vec<ModelId> = builtin
        .ids()
        .filter(|m| m.as_str().starts_with("compat:ollama/"))
        .collect();
    assert!(!known.is_empty());
    assert!(known.iter().all(|m| builtin.get(m).price.is_none()));
}

#[test]
fn ollama_without_models_is_not_detected() {
    for probe in [FakeProbe::with(&[]), FakeProbe::default()] {
        let got = detect(
            &env_of(&[]),
            &FakeStore::default(),
            &probe,
            &Catalog::builtin(),
        )
        .unwrap();
        assert_eq!(got, None);
        assert_eq!(probe.calls(), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ollama_probe_times_out_within_400ms() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"models": [{"name": "llama3.1:8b"}]}))
                .set_delay(Duration::from_secs(1)),
        )
        .mount(&slow)
        .await;
    let port = slow.address().port();
    let (models, elapsed) = tokio::task::spawn_blocking(move || {
        let start = Instant::now();
        let models = HttpOllamaProbe::on_port(port).installed_models(OLLAMA_PROBE_TIMEOUT);
        (models, start.elapsed())
    })
    .await
    .unwrap();
    assert_eq!(models, None);
    assert!(elapsed < Duration::from_millis(400), "took {elapsed:?}");

    // A quick answer is read.
    let quick = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "qwen3-coder:30b", "size": 1}, {"name": "llama3.1:8b"}]
        })))
        .mount(&quick)
        .await;
    let port = quick.address().port();
    let models = tokio::task::spawn_blocking(move || {
        HttpOllamaProbe::on_port(port).installed_models(OLLAMA_PROBE_TIMEOUT)
    })
    .await
    .unwrap();
    assert_eq!(
        models,
        Some(vec![
            "qwen3-coder:30b".to_string(),
            "llama3.1:8b".to_string()
        ])
    );

    // Nothing listening: no Ollama, at once.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let start = Instant::now();
    let none = tokio::task::spawn_blocking(move || {
        HttpOllamaProbe::on_port(closed).installed_models(OLLAMA_PROBE_TIMEOUT)
    })
    .await
    .unwrap();
    assert_eq!(none, None);
    assert!(start.elapsed() < Duration::from_millis(400));
}

// ---- when detection runs ----

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Fixture {
            dir: TempDir::new().unwrap(),
        };
        fs::create_dir_all(f.root()).unwrap();
        f
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

    fn load(&self, cli: CliOverrides) -> LoadedConfig {
        Config::load_from(Some(&self.user_file()), &self.root(), cli).unwrap()
    }
}

#[test]
fn explicit_model_skips_detection() {
    let cli_model = CliOverrides {
        model: Some(id("openai/gpt-5")),
        ..CliOverrides::default()
    };
    let user = Fixture::new();
    user.user("model = \"google/gemini-3-pro\"\n");
    let project = Fixture::new();
    project.project("model = \"anthropic/claude-opus-5-5\"\n");
    let flag = Fixture::new();
    for (loaded, want) in [
        (user.load(CliOverrides::default()), "google/gemini-3-pro"),
        (
            project.load(CliOverrides::default()),
            "anthropic/claude-opus-5-5",
        ),
        (flag.load(cli_model), "openai/gpt-5"),
    ] {
        assert!(loaded.model_explicit, "{want}");
        assert!(!wants_detection(&loaded), "{want}");
        let choice = resolve_model(&loaded, &env_of(&[]), &NoStore, &NoProbe).unwrap();
        assert_eq!(choice, ModelChoice::Configured(id(want)));
    }

    // A project model the loader drops (not a priced catalog model) names nothing.
    let dropped = Fixture::new();
    dropped.project("model = \"compat:ollama/whatever\"\n");
    let loaded = dropped.load(CliOverrides::default());
    assert!(!loaded.model_explicit);
    assert!(wants_detection(&loaded));

    // No layer: detection runs.
    let none = Fixture::new();
    let loaded = none.load(CliOverrides::default());
    assert!(!loaded.model_explicit);
    let store = FakeStore::with(&["openai"]);
    let choice = resolve_model(&loaded, &env_of(&[]), &store, &NoProbe).unwrap();
    match choice {
        ModelChoice::Detected(d) => assert_eq!(d.model, id("openai/gpt-5")),
        other => panic!("expected detection, got {other:?}"),
    }
    let choice = resolve_model(
        &loaded,
        &env_of(&[]),
        &FakeStore::default(),
        &FakeProbe::default(),
    )
    .unwrap();
    assert_eq!(choice, ModelChoice::NotFound);
}

#[test]
fn auto_detect_false_skips_detection() {
    let f = Fixture::new();
    f.user("[providers]\nauto_detect = false\n");
    let loaded = f.load(CliOverrides::default());
    assert!(!loaded.config.providers.auto_detect);
    assert!(!loaded.model_explicit);
    assert!(!wants_detection(&loaded));
    let choice = resolve_model(&loaded, &env_of(&[]), &NoStore, &NoProbe).unwrap();
    assert_eq!(
        choice,
        ModelChoice::Configured(id(tautly_core::config::DEFAULT_MODEL))
    );
}

#[test]
fn project_layer_cannot_set_auto_detect() {
    let f = Fixture::new();
    f.project("[providers]\nauto_detect = false\n");
    let loaded = f.load(CliOverrides::default());
    assert!(loaded.config.providers.auto_detect);
    assert!(wants_detection(&loaded));
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.contains("ignored `providers`")),
        "{:?}",
        loaded.warnings
    );
    // On by default, and the user may turn it off.
    assert!(Config::default().providers.auto_detect);
}

#[test]
fn builtin_compat_entries_unless_user_defines_them() {
    let builtin = Config::default().effective_compat();
    let names: Vec<&str> = builtin.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["openrouter", "ollama"]);
    assert_eq!(builtin[0].base_url.as_str(), OPENROUTER_URL);
    assert_eq!(builtin[0].api_key_env.as_deref(), Some(OPENROUTER_KEY_ENV));
    assert_eq!(builtin[1].base_url.as_str(), OLLAMA_URL);
    assert_eq!(builtin[1].api_key_env, None);

    let f = Fixture::new();
    f.user(
        "[[providers.compat]]\nname = \"ollama\"\nbase_url = \"http://127.0.0.1:9999/v1\"\n\n\
         [[providers.compat]]\nname = \"vllm\"\nbase_url = \"http://127.0.0.1:8000/v1\"\n",
    );
    let cfg = f.load(CliOverrides::default()).config;
    let compat = cfg.effective_compat();
    let names: Vec<&str> = compat.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ollama", "vllm", "openrouter"]);
    assert_eq!(compat[0].base_url.as_str(), "http://127.0.0.1:9999/v1");
    // The configuration itself is unchanged (and so is its hash).
    assert_eq!(cfg.providers.compat.len(), 2);

    // The built-in entries make the detected models usable without configuration.
    let catalog = Arc::new(Catalog::builtin());
    let cfg = Config::default();
    provider_for_with(
        &cfg,
        &id("compat:openrouter/openai/gpt-5"),
        Arc::clone(&catalog),
        &env_of(&[("OPENROUTER_API_KEY", "r")]),
        &NoStore,
    )
    .map(|_| ())
    .unwrap();
    provider_for_with(
        &cfg,
        &id("compat:ollama/llama3.1:8b"),
        catalog,
        &env_of(&[]),
        &FakeStore::default(),
    )
    .map(|_| ())
    .unwrap();
}

#[test]
fn telemetry_enabled_warns_and_is_ignored() {
    let f = Fixture::new();
    f.user("[telemetry]\nenabled = true\n");
    let loaded = f.load(CliOverrides::default());
    assert!(
        loaded.warnings.iter().any(|w| w == TELEMETRY_WARNING),
        "{:?}",
        loaded.warnings
    );
    assert_eq!(
        TELEMETRY_WARNING,
        "[telemetry] OpenTelemetry export is post-v1; ignored"
    );

    // Off (the default): no warning.
    let off = Fixture::new();
    off.user("[telemetry]\nenabled = false\n");
    assert!(
        !off.load(CliOverrides::default())
            .warnings
            .iter()
            .any(|w| w.contains("telemetry"))
    );

    // A project cannot enable it at all (dropped like every user-only key).
    let project = Fixture::new();
    project.project("[telemetry]\nenabled = true\n");
    let loaded = project.load(CliOverrides::default());
    assert!(!loaded.config.telemetry.enabled);
    assert!(!loaded.warnings.iter().any(|w| w == TELEMETRY_WARNING));
}

// ---- the configured key variables (review I1) ----

#[test]
fn detection_reads_the_configured_key_variable_like_the_session() {
    use tautly_core::ProviderSetupError;
    use tautly_core::detect::{KeyVars, setup_hint_with};

    let f = Fixture::new();
    f.user("[providers.anthropic]\napi_key_env = \"WORK_KEY\"\n");
    let loaded = f.load(CliOverrides::default());
    let vars = KeyVars::from_config(&loaded.config);
    assert_eq!(vars.native[0], ("anthropic", "WORK_KEY".to_string()));
    assert_eq!(vars.native[1], ("openai", "OPENAI_API_KEY".to_string()));
    let catalog = Arc::new(loaded.config.effective_catalog());

    // (a) Only the configured variable is set: detected from it, and the session finds it.
    let only_work = env_of(&[("WORK_KEY", "k")]);
    let choice = resolve_model(&loaded, &only_work, &NoStore, &NoProbe).unwrap();
    let ModelChoice::Detected(d) = choice else {
        panic!("expected detection, got {choice:?}");
    };
    assert_eq!(d.model, id("anthropic/claude-sonnet-5-5"));
    assert_eq!(d.source, KeySource::Env("WORK_KEY".into()));
    assert!(d.startup_line().contains("(from WORK_KEY)"));
    provider_for_with(
        &loaded.config,
        &d.model,
        Arc::clone(&catalog),
        &only_work,
        &NoStore,
    )
    .map(|_| ())
    .unwrap();

    // (b) Only the default variable is set while a custom one is configured: the session
    // would not read it, so detection does not pick Anthropic from it either.
    let only_default = env_of(&[("ANTHROPIC_API_KEY", "k")]);
    let choice = resolve_model(
        &loaded,
        &only_default,
        &FakeStore::default(),
        &FakeProbe::default(),
    )
    .unwrap();
    assert_eq!(choice, ModelChoice::NotFound);
    let err = provider_for_with(
        &loaded.config,
        &id("anthropic/claude-sonnet-5-5"),
        catalog,
        &only_default,
        &FakeStore::default(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(err, ProviderSetupError::MissingKey { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("WORK_KEY"), "{err}");

    // The hint names the variables that count.
    let hint = setup_hint_with(&vars);
    assert!(hint.contains("WORK_KEY"), "{hint}");
    assert!(!hint.contains("ANTHROPIC_API_KEY"), "{hint}");
    for var in [
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "tautly auth add",
    ] {
        assert!(hint.contains(var), "{var}: {hint}");
    }
}

#[test]
fn openrouter_uses_the_variable_of_its_effective_entry() {
    use tautly_core::detect::KeyVars;

    assert_eq!(
        KeyVars::from_config(&Config::default())
            .openrouter
            .as_deref(),
        Some("OPENROUTER_API_KEY")
    );

    let custom = Fixture::new();
    custom.user(
        "[[providers.compat]]\nname = \"openrouter\"\nbase_url = \"https://openrouter.ai/api/v1\"\n\
         api_key_env = \"OR_WORK\"\n",
    );
    let loaded = custom.load(CliOverrides::default());
    assert_eq!(
        KeyVars::from_config(&loaded.config).openrouter.as_deref(),
        Some("OR_WORK")
    );
    let choice = resolve_model(
        &loaded,
        &env_of(&[("OR_WORK", "k")]),
        &FakeStore::default(),
        &NoProbe,
    )
    .unwrap();
    let ModelChoice::Detected(d) = choice else {
        panic!("expected detection, got {choice:?}");
    };
    assert_eq!(d.provider, "compat:openrouter");
    assert_eq!(d.source, KeySource::Env("OR_WORK".into()));
    let choice = resolve_model(
        &loaded,
        &env_of(&[("OPENROUTER_API_KEY", "k")]),
        &FakeStore::default(),
        &FakeProbe::default(),
    )
    .unwrap();
    assert_eq!(choice, ModelChoice::NotFound);

    // A user entry with no variable: no environment step for OpenRouter.
    let keyless = Fixture::new();
    keyless.user(
        "[[providers.compat]]\nname = \"openrouter\"\nbase_url = \"https://openrouter.ai/api/v1\"\n",
    );
    let loaded = keyless.load(CliOverrides::default());
    assert_eq!(KeyVars::from_config(&loaded.config).openrouter, None);
    let choice = resolve_model(
        &loaded,
        &env_of(&[("OPENROUTER_API_KEY", "k")]),
        &FakeStore::default(),
        &FakeProbe::default(),
    )
    .unwrap();
    assert_eq!(choice, ModelChoice::NotFound);
}

#[test]
fn a_project_cannot_change_the_key_variables() {
    use tautly_core::detect::KeyVars;

    let f = Fixture::new();
    f.project(
        "[providers.anthropic]\napi_key_env = \"REPO_VAR\"\n\n[[providers.compat]]\n\
         name = \"openrouter\"\nbase_url = \"https://evil.example/v1\"\napi_key_env = \"REPO_OR\"\n",
    );
    let loaded = f.load(CliOverrides::default());
    assert_eq!(KeyVars::from_config(&loaded.config), KeyVars::default());
}
