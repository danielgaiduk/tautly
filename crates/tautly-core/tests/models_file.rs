//! `models.toml` next to the user config file (written by `tautly models update`, Task 31)
//! adds catalog entries; the user file's own `[catalog_overrides]` win over it.

use std::fs;
use std::path::PathBuf;

use tautly_core::{CliOverrides, Config, LoadedConfig};
use tautly_provider::ModelId;
use tempfile::TempDir;

const GATEWAY_MODEL: &str = "openai/gateway-1";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fx = Fixture {
            dir: TempDir::new().unwrap(),
        };
        fs::create_dir_all(fx.user_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fx.dir.path().join("project")).unwrap();
        fx
    }

    fn user_file(&self) -> PathBuf {
        self.dir.path().join("user").join("config.toml")
    }

    fn models_file(&self) -> PathBuf {
        self.user_file().with_file_name("models.toml")
    }

    fn load(&self) -> LoadedConfig {
        Config::load_from(
            Some(&self.user_file()),
            &self.dir.path().join("project"),
            CliOverrides::default(),
        )
        .unwrap()
    }
}

fn model_entry(window: u32, price_input: f64) -> String {
    format!(
        "[models.\"{GATEWAY_MODEL}\"]\ncontext_window = {window}\nmax_output = 4000\n\
         parallel_tools = true\nreasoning = \"none\"\ncache = \"none\"\nimages = false\n\
         edit_format = \"str_replace\"\n[models.\"{GATEWAY_MODEL}\".price]\n\
         input = {price_input}\noutput = 2.0\ncache_read = 0.1\ncache_write = 1.0\n"
    )
}

#[test]
fn models_file_next_to_the_user_file_adds_catalog_entries() {
    let fx = Fixture::new();
    fs::write(fx.user_file(), "").unwrap();
    fs::write(fx.models_file(), model_entry(77_000, 1.5)).unwrap();
    let id = ModelId::from(GATEWAY_MODEL);
    let catalog = fx.load().config.catalog_overrides.expect("applied");
    assert_eq!(catalog.get(&id).caps.context_window, 77_000);
    assert_eq!(catalog.get(&id).price.unwrap().input, 1.5);
    // It works without a config.toml at all.
    fs::remove_file(fx.user_file()).unwrap();
    assert!(fx.load().config.catalog_overrides.is_some());
}

#[test]
fn user_overrides_win_and_a_bad_models_file_is_a_warning() {
    let fx = Fixture::new();
    fs::write(
        fx.user_file(),
        model_entry(1234, 9.0).replace("[models.", "[catalog_overrides.models."),
    )
    .unwrap();
    fs::write(fx.models_file(), model_entry(77_000, 1.5)).unwrap();
    let info = fx
        .load()
        .config
        .catalog_overrides
        .unwrap()
        .get(&ModelId::from(GATEWAY_MODEL));
    assert_eq!(info.caps.context_window, 1234, "config.toml wins");
    assert_eq!(info.price.unwrap().input, 9.0);

    for bad in [
        "this is [not toml",
        "[models.\"x/y\"]\ncontext_window = \"big\"\n",
    ] {
        fs::write(fx.models_file(), bad).unwrap();
        let loaded = fx.load();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("model catalog file not read")),
            "{bad}: {:?}",
            loaded.warnings
        );
    }
}
