use hx_provider::*;

fn u(input: u64, output: u64, cache_read: u64, cache_write: u64) -> RawUsage {
    RawUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

#[test]
fn builtin_catalog_has_each_provider_family() {
    for id in [
        "anthropic/claude-sonnet-5-5",
        "openai/gpt-5",
        "google/gemini-3-pro",
    ] {
        assert!(Catalog::builtin().contains(&id.into()), "{id}");
    }
}

#[test]
fn builtin_has_all_required_models() {
    let c = Catalog::builtin();
    for id in [
        "anthropic/claude-sonnet-5-5",
        "anthropic/claude-opus-5-5",
        "anthropic/claude-haiku-5-5",
        "anthropic/claude-fable-5-1",
        "openai/gpt-5",
        "openai/gpt-5-mini",
        "google/gemini-3-pro",
        "google/gemini-3-flash",
    ] {
        assert!(c.contains(&id.into()), "{id}");
        assert!(c.get(&id.into()).price.is_some(), "{id} price");
    }
}

#[test]
fn default_edit_formats_match_spec() {
    let c = Catalog::builtin();
    assert_eq!(
        c.get(&"anthropic/claude-sonnet-5-5".into())
            .caps
            .edit_format,
        EditFormat::StrReplace
    );
    assert_eq!(
        c.get(&"openai/gpt-5".into()).caps.edit_format,
        EditFormat::Patch
    );
    assert_eq!(
        c.get(&"google/gemini-3-pro".into()).caps.edit_format,
        EditFormat::StrReplace
    );
    assert_eq!(
        c.get(&"compat/some-local".into()).caps.edit_format,
        EditFormat::Hashline
    );
}

#[test]
fn unknown_model_gets_conservative_defaults() {
    let i = Catalog::builtin().get(&"compat/some-local".into());
    assert_eq!(
        i.caps,
        Capabilities {
            context_window: 32_000,
            max_output: 4_000,
            parallel_tools: false,
            reasoning: ReasoningStyle::None,
            cache: CacheStyle::None,
            images: false,
            edit_format: EditFormat::Hashline,
        }
    );
    assert_eq!(i.price, None);
}

#[test]
fn unknown_model_cost_is_none() {
    assert_eq!(cost_usd(&u(1000, 1000, 0, 0), None), None);
}

#[test]
fn cost_math() {
    let p = Price {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    };
    assert!(
        (cost_usd(&u(1_000_000, 100_000, 2_000_000, 0), Some(&p)).unwrap() - (3.0 + 1.5 + 0.6))
            .abs()
            < 1e-9
    );
}

#[test]
fn override_merges_by_id() {
    let mut c = Catalog::builtin();
    let before = c.get(&"openai/gpt-5".into());
    let sonnet_before = c.get(&"anthropic/claude-sonnet-5-5".into());
    let o = Catalog::from_toml(
        r#"
[models."anthropic/claude-sonnet-5-5"]
context_window = 100000
max_output = 8000
parallel_tools = false
reasoning = "none"
cache = "none"
images = false
edit_format = "hashline"
[models."anthropic/claude-sonnet-5-5".price]
input = 1.0
output = 2.0
cache_read = 0.1
cache_write = 0.2
[models."compat:local/x"]
context_window = 8000
max_output = 1000
parallel_tools = false
reasoning = "none"
cache = "none"
images = false
edit_format = "hashline"
"#,
    )
    .unwrap();
    c.merge(o);
    let s = c.get(&"anthropic/claude-sonnet-5-5".into());
    assert_ne!(s, sonnet_before);
    assert_eq!(s.price.unwrap().input, 1.0);
    assert_eq!(s.caps.context_window, 100_000);
    assert_eq!(c.get(&"openai/gpt-5".into()), before);
    assert!(c.contains(&"compat:local/x".into()));
    assert_eq!(c.get(&"compat:local/x".into()).price, None);
}

#[test]
fn model_id_parts_and_serde() {
    let a = ModelId::from("anthropic/claude-sonnet-5-5");
    assert_eq!(
        (a.provider(), a.model()),
        ("anthropic", "claude-sonnet-5-5")
    );
    let c = ModelId::from("compat:ollama/llama3");
    assert_eq!((c.provider(), c.model()), ("compat", "llama3"));
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        "\"anthropic/claude-sonnet-5-5\""
    );
}

#[test]
fn image_bytes_serialize_as_base64() {
    let b = Block::Image {
        media_type: "image/png".into(),
        data: vec![1, 2, 3],
    };
    let j = serde_json::to_value(&b).unwrap();
    assert_eq!(j["image"]["data"], "AQID");
    assert_eq!(serde_json::from_value::<Block>(j).unwrap(), b);
}

#[test]
fn only_transient_errors_are_transient() {
    assert!(
        ProviderError::Transient {
            status: Some(529),
            retry_after: None,
            msg: "x".into()
        }
        .is_transient()
    );
    assert!(!ProviderError::Auth("x".into()).is_transient());
}
