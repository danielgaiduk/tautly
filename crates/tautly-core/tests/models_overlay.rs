//! A models file entry of a built-in model may hold only the fields `tautly models update`
//! changes; the rest is the running version's built-in entry (Task 31, review I3).

use std::fs;

use tautly_core::{CliOverrides, Config, parse_models_file};
use tautly_provider::{Catalog, ModelId};
use tempfile::TempDir;

const PARTIAL: &str = "[models.\"anthropic/claude-sonnet-5-5\"]\n\
    context_window = 123456\nmax_output = 7777\nimages = false\n\
    [models.\"anthropic/claude-sonnet-5-5\".price]\n\
    input = 2.5\noutput = 12.0\ncache_read = 0.25\ncache_write = 3.0\n";

#[test]
fn a_partial_entry_of_a_builtin_model_keeps_the_builtin_capabilities() {
    let id = ModelId::from("anthropic/claude-sonnet-5-5");
    let builtin = Catalog::builtin().get(&id);
    let parsed = parse_models_file(PARTIAL).unwrap().get(&id);
    assert_eq!(parsed.caps.context_window, 123_456);
    assert_eq!(parsed.caps.max_output, 7777);
    assert!(!parsed.caps.images);
    assert_eq!(parsed.price.unwrap().input, 2.5);
    // Everything the file does not say is the built-in entry's.
    assert_eq!(parsed.caps.reasoning, builtin.caps.reasoning);
    assert_eq!(parsed.caps.cache, builtin.caps.cache);
    assert_eq!(parsed.caps.edit_format, builtin.caps.edit_format);
    assert_eq!(parsed.cache_ttl_s, builtin.cache_ttl_s);

    // The loader applies it the same way.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("project")).unwrap();
    fs::write(dir.path().join("config.toml"), "").unwrap();
    fs::write(dir.path().join("models.toml"), PARTIAL).unwrap();
    let loaded = Config::load_from(
        Some(&dir.path().join("config.toml")),
        &dir.path().join("project"),
        CliOverrides::default(),
    )
    .unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let mut catalog = Catalog::builtin();
    catalog.merge(loaded.config.catalog_overrides.unwrap());
    let info = catalog.get(&id);
    assert_eq!(info.caps.context_window, 123_456);
    assert_eq!(info.caps.edit_format, builtin.caps.edit_format);
}

#[test]
fn an_unknown_model_needs_a_full_entry_and_empty_files_are_empty_catalogs() {
    assert!(parse_models_file("[models.\"x/y\"]\ncontext_window = 10\n").is_err());
    assert_eq!(parse_models_file("# nothing\n").unwrap().ids().count(), 0);
    assert!(parse_models_file("not [toml").is_err());
}

#[test]
fn the_config_hash_is_the_user_and_project_layers_without_the_models_file() {
    // cp5 review C M10 (spec 6.5): not moved by `tautly models update` (the models file)
    // or by the built-in values of the running version; moved by what the files set.
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join(".tautly")).unwrap();
    let user = dir.path().join("config.toml");
    let hash = || {
        Config::load_from(Some(&user), &project, CliOverrides::default())
            .unwrap()
            .config
            .hash()
    };
    fs::write(&user, "max_turns = 30\n").unwrap();
    let plain = hash();
    fs::write(dir.path().join("models.toml"), PARTIAL).unwrap();
    assert_eq!(hash(), plain, "the models file is not hashed");
    // Equal for equal settings, whatever the file layout.
    fs::write(&user, "# a comment\nmax_turns   =   30\n").unwrap();
    assert_eq!(hash(), plain);
    fs::write(&user, "max_turns = 31\n").unwrap();
    assert_ne!(hash(), plain);
    fs::write(&user, "max_turns = 30\n").unwrap();
    fs::write(project.join(".tautly/config.toml"), "max_turns = 10\n").unwrap();
    assert_ne!(hash(), plain, "the project layer is hashed");
}
