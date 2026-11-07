//! `tautly models list|update` (spec 4.3): the model catalog, and refreshing it from a
//! models.dev-format document into `<config home>/models.toml`, which the configuration
//! loader applies over the built-in catalog (the user's `[catalog_overrides]` win over it).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::Subcommand;
use serde_json::Value;
use tautly_core::{
    Catalog, CliOverrides, Config, MODELS_FILE, ModelId, ModelInfo, Price, parse_models_file,
};

use super::Env;

/// Where `models update` reads from unless `--from` says otherwise.
pub const DEFAULT_SOURCE: &str = "https://models.dev/api.json";
/// The providers Tautly has adapters for: their models.dev keys are the catalog prefixes.
const PROVIDERS: &[&str] = &["anthropic", "openai", "google"];
/// Most bytes of the document read.
const MAX_BYTES: usize = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Every model of the catalog: window, output limit and price per million tokens.
    List,
    /// Refresh windows and prices from a models.dev-format file, into models.toml.
    Update {
        /// The URL to read.
        #[arg(long, value_name = "URL", default_value = DEFAULT_SOURCE)]
        from: String,
    },
}

pub async fn run(cmd: Cmd, env: &Env) -> anyhow::Result<i32> {
    match cmd {
        Cmd::List => list(env),
        Cmd::Update { from } => update(&from).await,
    }
}

fn list(env: &Env) -> anyhow::Result<i32> {
    let loaded = Config::load(env.root(), CliOverrides::default())?;
    for warning in &loaded.warnings {
        eprintln!("tautly: {}", tautly_core::printable_multiline(warning));
    }
    let overrides = loaded.config.catalog_overrides;
    let mut catalog = Catalog::builtin();
    if let Some(o) = overrides.clone() {
        catalog.merge(o);
    }
    let ids: Vec<ModelId> = catalog.ids().collect();
    let width = ids
        .iter()
        .map(|i| i.as_str().len())
        .max()
        .unwrap_or(2)
        .max(2);
    println!(
        "{:<width$}  {:>9}  {:>8}  {:>8}  {:>8}  {:<6}  SOURCE",
        "ID", "WINDOW", "MAX OUT", "IN $/M", "OUT $/M", "TIER"
    );
    for id in ids {
        let info = catalog.get(&id);
        let (input, output) = info.price.map_or_else(
            || ("-".to_string(), "-".to_string()),
            |p| (p.input.to_string(), p.output.to_string()),
        );
        let tier = info
            .tier
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "-".into());
        let source = if overrides.as_ref().is_some_and(|o| o.contains(&id)) {
            "user"
        } else {
            "built-in"
        };
        println!(
            "{:<width$}  {:>9}  {:>8}  {input:>8}  {output:>8}  {tier:<6}  {source}",
            id.as_str(),
            info.caps.context_window,
            info.caps.max_output
        );
    }
    Ok(0)
}

async fn update(from: &str) -> anyhow::Result<i32> {
    let dir = tautly_core::config_home().map_err(|e| anyhow!("no config directory: {e}"))?;
    let file = dir.join(MODELS_FILE);
    let existing = match std::fs::read_to_string(&file) {
        Ok(text) => parse_models_file(&text).map_err(|e| {
            anyhow!(
                "{} is not a model catalog ({e}); fix or delete it, then update again",
                file.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog::default(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
    };
    let doc = fetch(from).await?;
    let merged = merge(&Catalog::builtin(), &existing, &doc);
    let text = render(from, &Catalog::builtin(), &merged.entries)?;
    write_atomically(&file, &text)?;
    println!(
        "{} updated, {} added, {} unchanged (from {from})",
        merged.updated, merged.added, merged.unchanged
    );
    println!(
        "wrote {} model{} to {}",
        merged.entries.len(),
        if merged.entries.len() == 1 { "" } else { "s" },
        file.display()
    );
    Ok(0)
}

/// The document at `url`, as JSON.
async fn fetch(url: &str) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("tautly/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| anyhow!("cannot read {url}: {}", e.without_url()))?;
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow!("cannot read {url}: {}", e.without_url()))?
    {
        if body.len() + chunk.len() > MAX_BYTES {
            bail!("{url} is larger than {} MB", MAX_BYTES / (1024 * 1024));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).with_context(|| format!("{url} is not JSON"))
}

/// What the document changes.
struct Merged {
    /// The models.toml content: id and its info, sorted by id.
    entries: BTreeMap<String, ModelInfo>,
    updated: u32,
    added: u32,
    unchanged: u32,
}

/// The entries of `existing` (the current models.toml, as the loader reads it) with the
/// models of `doc` applied: a built-in model takes the document's window, output limit,
/// image support and price over its built-in entry (the rest is the running version's, so
/// `render` writes only these fields); a model only the file knows keeps its entry's other
/// fields; one nobody knows is added with conservative capabilities (no reasoning or cache
/// control, hashline edits, no parallel tools). Only models of providers Tautly has
/// adapters for that can call tools and have a price and limits are used. An entry that
/// equals the built-in one is not written.
fn merge(builtin: &Catalog, existing: &Catalog, doc: &Value) -> Merged {
    let mut entries: BTreeMap<String, ModelInfo> = existing
        .ids()
        .map(|id| {
            let info = existing.get(&id);
            let info = if builtin.contains(&id) {
                persisted_over(builtin.get(&id), &info)
            } else {
                info
            };
            (id.as_str().to_string(), info)
        })
        .collect();
    let (mut updated, mut added, mut unchanged) = (0, 0, 0);
    for provider in PROVIDERS {
        let Some(models) = doc
            .get(provider)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (key, m) in models {
            let name = m.get("id").and_then(Value::as_str).unwrap_or(key);
            let id = format!("{provider}/{name}");
            let Some(source) = Source::read(m, provider).filter(|_| plain_id(&id)) else {
                continue;
            };
            let model = ModelId::from(id.as_str());
            let in_builtin = builtin.contains(&model);
            let known = entries.contains_key(&id) || in_builtin;
            // What the model is now (for the counts), and what to build on: a built-in
            // model on its built-in entry, never on a copy of it from an older update.
            let before = entries
                .get(&id)
                .cloned()
                .unwrap_or_else(|| builtin.get(&model));
            let base = if in_builtin {
                builtin.get(&model)
            } else {
                before.clone()
            };
            let after = source.apply(base, known);
            match (known, after == before) {
                (true, true) => unchanged += 1,
                (true, false) => updated += 1,
                (false, _) => added += 1,
            }
            entries.insert(id, after);
        }
    }
    // An entry the built-in catalog has, unchanged, is no override.
    entries.retain(|id, info| {
        let id = ModelId::from(id.as_str());
        !builtin.contains(&id) || builtin.get(&id) != *info
    });
    Merged {
        entries,
        updated,
        added,
        unchanged,
    }
}

/// `base` (a built-in entry) with the fields `update` keeps for a built-in model taken from
/// `entry`: the window, output limit, image support and price.
fn persisted_over(mut base: ModelInfo, entry: &ModelInfo) -> ModelInfo {
    base.caps.context_window = entry.caps.context_window;
    base.caps.max_output = entry.caps.max_output;
    base.caps.images = entry.caps.images;
    base.price = entry.price;
    base
}

/// The numbers of one models.dev model.
struct Source {
    context: u32,
    output: u32,
    images: Option<bool>,
    input: f64,
    price_output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    /// The cache write price when the source gives none: Anthropic charges 1.25 times the
    /// input price for a cache write, the others bill no extra.
    default_cache_write: f64,
}

impl Source {
    fn read(m: &Value, provider: &str) -> Option<Source> {
        if m.get("tool_call").and_then(Value::as_bool) == Some(false) {
            return None;
        }
        let limit = |k: &str| {
            m.pointer(&format!("/limit/{k}"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
        };
        let cost = |k: &str| {
            m.pointer(&format!("/cost/{k}"))
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        let input = cost("input")?;
        Some(Source {
            context: limit("context")?,
            output: limit("output")?,
            images: m
                .pointer("/modalities/input")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|v| v.as_str() == Some("image"))),
            input,
            price_output: cost("output")?,
            cache_read: cost("cache_read"),
            cache_write: cost("cache_write"),
            default_cache_write: if provider == "anthropic" {
                input * 1.25
            } else {
                input
            },
        })
    }

    /// `info` with these numbers; for a model the catalog does not know, `info` is the
    /// conservative default and the document's image support is kept.
    fn apply(&self, mut info: ModelInfo, known: bool) -> ModelInfo {
        info.caps.context_window = self.context;
        info.caps.max_output = self.output;
        if let Some(images) = self.images {
            info.caps.images = images;
        }
        // A cache price the source does not give stays what the entry has; for a new price
        // it is the input price (a read) or the provider's usual write price.
        let (read, write) = match info.price.filter(|_| known) {
            Some(p) => (
                self.cache_read.unwrap_or(p.cache_read),
                self.cache_write.unwrap_or(p.cache_write),
            ),
            None => (
                self.cache_read.unwrap_or(self.input),
                self.cache_write.unwrap_or(self.default_cache_write),
            ),
        };
        info.price = Some(Price {
            input: self.input,
            output: self.price_output,
            cache_read: read,
            cache_write: write,
        });
        info
    }
}

/// The snake_case name serde gives a unit variant, as a `String`.
macro_rules! name {
    ($v:expr) => {
        serde_json::to_value($v)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
}

/// Ids that go into a TOML key without escaping trouble.
fn plain_id(id: &str) -> bool {
    id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

/// The file text: a header, then one `[models."<id>"]` table per entry: the window, output
/// limit, image support and price for a built-in model (its other fields are the running
/// version's, see `tautly_core::parse_models_file`), the whole entry for any other. Checked
/// by reading it back as the loader does.
fn render(
    from: &str,
    builtin: &Catalog,
    entries: &BTreeMap<String, ModelInfo>,
) -> anyhow::Result<String> {
    let mut out = format!(
        "# Written by `tautly models update` from {from}.\n\
         # Entries here replace the built-in catalog's (and `[catalog_overrides]` in\n\
         # config.toml replaces these); delete the file to go back to the built-in catalog.\n\
         # An entry of a built-in model holds only what update changes (window, output limit,\n\
         # images, prices): the rest comes from the built-in entry, and other edits belong in\n\
         # `[catalog_overrides]` of config.toml.\n"
    );
    for (id, info) in entries {
        let c = &info.caps;
        let _ = write!(
            out,
            "\n[models.{id:?}]\ncontext_window = {}\nmax_output = {}\nimages = {}\n",
            c.context_window, c.max_output, c.images
        );
        if !builtin.contains(&ModelId::from(id.as_str())) {
            let _ = write!(
                out,
                "parallel_tools = {}\nreasoning = {:?}\ncache = {:?}\nedit_format = {:?}\n",
                c.parallel_tools,
                name!(&c.reasoning),
                name!(&c.cache),
                name!(&c.edit_format),
            );
            if let Some(tier) = &info.tier {
                let _ = writeln!(out, "tier = {:?}", name!(tier));
            }
            if let (Some(ttl), false) = (info.cache_ttl_s, name!(&c.cache) == "none") {
                let _ = writeln!(out, "cache_ttl_s = {ttl}");
            }
        }
        if let Some(p) = info.price {
            let _ = write!(
                out,
                "[models.{id:?}.price]\ninput = {:?}\noutput = {:?}\ncache_read = {:?}\n\
                 cache_write = {:?}\n",
                p.input, p.output, p.cache_read, p.cache_write
            );
        }
    }
    let parsed =
        parse_models_file(&out).map_err(|e| anyhow!("the new catalog does not read back: {e}"))?;
    for (id, info) in entries {
        let id = ModelId::from(id.as_str());
        if parsed.get(&id) != *info {
            bail!("the new catalog entry of {id} does not read back as it was built");
        }
    }
    Ok(out)
}

/// Write `text` to `file` through a temporary file in the same directory.
fn write_atomically(file: &Path, text: &str) -> anyhow::Result<()> {
    let dir = file.parent().context("the catalog file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".models.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("cannot write {}: {e}", file.display())
    })
}
