use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::DocumentMut;

use crate::config::Config;

/// Serialize `Config` to TOML with canonical hierarchical spacing:
///
/// - 2 blank lines between top-level sections
/// - 2 blank lines between sibling `providers.*` / `models.*` entries
/// - 1 blank line between sub-tables within a provider/model entry
///
/// The output is deterministic: the same `Config` always produces the same bytes.
pub fn canonical_format(cfg: &Config) -> Result<String> {
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config")?;
    let mut doc = serialized
        .parse::<DocumentMut>()
        .context("failed to parse serialized config for canonical formatting")?;
    apply_canonical_decor(&mut doc);
    Ok(doc.to_string())
}

fn apply_canonical_decor(doc: &mut DocumentMut) {
    for (key, item) in doc.iter_mut() {
        if let Some(table) = item.as_table_mut() {
            // toml_edit serializes an extra \n after each table body,
            // so effective blank lines = prefix \n count − 1.
            // 2 blank lines between top-level sections → "\n\n"
            table.decor_mut().set_prefix("\n\n");
            let key_str = key.get().to_string();
            if key_str == "providers" || key_str == "models" {
                // Sibling entries (providers.X, models.Y) also get 2 blank lines
                format_entry_children(table);
            } else {
                format_sub_tables(table);
            }
        }
    }
}

/// Format children of `providers`/`models`: each entry gets 2 blank lines,
/// each entry's own sub-tables get 1 blank line.
fn format_entry_children(table: &mut toml_edit::Table) {
    for (_key, item) in table.iter_mut() {
        if let Some(entry) = item.as_table_mut() {
            entry.decor_mut().set_prefix("\n\n");
            format_sub_tables(entry);
        }
    }
}

/// Sub-tables within a single provider/model entry: 1 blank line.
fn format_sub_tables(table: &mut toml_edit::Table) {
    for (_key, item) in table.iter_mut() {
        if let Some(sub) = item.as_table_mut() {
            sub.decor_mut().set_prefix("\n");
            format_deep_tables(sub);
        }
    }
}

/// Deeper nested tables: 1 blank line.
fn format_deep_tables(table: &mut toml_edit::Table) {
    for (_key, item) in table.iter_mut() {
        if let Some(sub) = item.as_table_mut() {
            sub.decor_mut().set_prefix("\n");
            format_deep_tables(sub);
        }
    }
}

pub fn write_full_config(path: &Path, cfg: &Config) -> Result<()> {
    cfg.validate()?;
    let serialized = canonical_format(cfg)?;
    atomic_write(path, serialized.as_bytes())
}

pub fn write_provider(path: &Path, cfg: &Config, provider_id: &str) -> Result<()> {
    cfg.validate()?;
    let serialized = canonical_format(cfg)?;
    if !path.exists() {
        atomic_write(path, serialized.as_bytes())?;
        return Ok(());
    }
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let source = serialized
        .parse::<DocumentMut>()
        .context("failed to parse serialized config for round-trip update")?;
    doc["providers"][provider_id] = source["providers"][provider_id].clone();
    write_doc(path, doc)
}

pub fn remove_provider(path: &Path, cfg: &Config, provider_id: &str) -> Result<()> {
    cfg.validate()?;
    if !path.exists() {
        return write_full_config(path, cfg);
    }
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if let Some(providers) = doc
        .get_mut("providers")
        .and_then(|item| item.as_table_like_mut())
    {
        providers.remove(provider_id);
    }
    // A forced removal also drops the provider from model bindings
    // in `cfg`; sync those model tables into the round-trip document, otherwise
    // the still-referencing file content is rejected by the validation below.
    let affected_models = models_referencing(&original, provider_id)?;
    if !affected_models.is_empty() {
        let serialized = canonical_format(cfg)?;
        let source = serialized
            .parse::<DocumentMut>()
            .context("failed to parse serialized config for round-trip update")?;
        for model_id in &affected_models {
            replace_model_table(&mut doc, &source, model_id);
        }
    }
    write_doc(path, doc)
}

/// Model IDs whose provider bindings in `original` reference `provider_id`.
fn models_referencing(original: &str, provider_id: &str) -> Result<Vec<String>> {
    let file_cfg: Config =
        toml::from_str(original).context("round-trip config update produced invalid TOML")?;
    Ok(file_cfg
        .models
        .iter()
        .filter(|(_, model)| {
            model
                .openai_chat_providers
                .iter()
                .chain(model.openai_responses_providers.iter())
                .chain(model.anthropic_providers.iter())
                .any(|binding| binding.name == provider_id)
        })
        .map(|(model_id, _)| model_id.clone())
        .collect())
}

pub fn write_models(path: &Path, cfg: &Config, model_ids: &[String]) -> Result<()> {
    cfg.validate()?;
    if !path.exists() {
        return write_full_config(path, cfg);
    }
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let serialized = canonical_format(cfg)?;
    let source = serialized
        .parse::<DocumentMut>()
        .context("failed to parse serialized config for round-trip update")?;

    let all_models_exist = model_ids.iter().all(|model_id| {
        doc.get("models")
            .and_then(|item| item.as_table_like())
            .is_some_and(|models| models.contains_key(model_id))
    });
    if !all_models_exist {
        return write_full_config(path, cfg);
    }
    for model_id in model_ids {
        replace_model_table(&mut doc, &source, model_id);
    }
    write_doc(path, doc)
}

pub fn write_model(path: &Path, cfg: &Config, model_id: &str) -> Result<()> {
    cfg.validate()?;
    let serialized = canonical_format(cfg)?;
    if !path.exists() {
        atomic_write(path, serialized.as_bytes())?;
        return Ok(());
    }
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let source = serialized
        .parse::<DocumentMut>()
        .context("failed to parse serialized config for round-trip update")?;
    let model_exists = doc
        .get("models")
        .and_then(|item| item.as_table_like())
        .is_some_and(|models| models.contains_key(model_id));
    if !model_exists {
        // toml_edit cannot safely transplant a new model table plus all nested
        // array-of-table provider bindings by assigning only one Item. Fall
        // back to a full validated rewrite for newly-created models; existing
        // model edits still use the localized round-trip path below.
        return write_full_config(path, cfg);
    }
    replace_model_table(&mut doc, &source, model_id);
    write_doc(path, doc)
}

fn replace_model_table(doc: &mut DocumentMut, source: &DocumentMut, model_id: &str) {
    if let (Some(existing), Some(source_model)) = (
        doc["models"][model_id].as_table_like_mut(),
        source["models"][model_id].as_table_like(),
    ) {
        let stale_keys = existing
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| !source_model.contains_key(key))
            .collect::<Vec<_>>();
        for key in stale_keys {
            existing.remove(&key);
        }
        for (key, value) in source_model.iter() {
            existing.insert(key, value.clone());
        }
    } else {
        doc["models"][model_id] = source["models"][model_id].clone();
    }
}

pub fn remove_model(path: &Path, cfg: &Config, model_id: &str) -> Result<()> {
    cfg.validate()?;
    if !path.exists() {
        return write_full_config(path, cfg);
    }
    let original = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if let Some(models) = doc
        .get_mut("models")
        .and_then(|item| item.as_table_like_mut())
    {
        models.remove(model_id);
    }
    write_doc(path, doc)
}

fn write_doc(path: &Path, doc: DocumentMut) -> Result<()> {
    let updated = doc.to_string();
    let roundtrip_cfg: Config =
        toml::from_str(&updated).context("round-trip config update produced invalid TOML")?;
    roundtrip_cfg
        .validate()
        .context("round-trip config update produced invalid config")?;
    let canonical = canonical_format(&roundtrip_cfg)?;
    atomic_write(path, canonical.as_bytes())
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("llm-proxy")
    ));
    std::fs::write(&tmp, data).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            path.display(),
            tmp.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config_toml() -> &'static str {
        r#"
[server]
listen = "127.0.0.1:8989"

[fallback]
chain = []

[providers.deepseek]
product = "deepseek"
api_key_env = "DEEPSEEK_API_KEY"

[providers.deepseek.openai_chat]
url = "https://api.deepseek.com/v1/chat"

[providers.deepseek.openai_chat.compat]
extra_headers = {}

[providers.provider-b]
product = "custom"
api_key_env = "PB_KEY"

[providers.provider-b.openai_chat]
url = "http://pb.example.com/v1/chat"

[models.model-a]
context_window = 4096
max_output_tokens = 1024
openai_chat_providers = [{ name = "deepseek", model = "ds-model" }]

[models.model-b]
context_window = 8192
max_output_tokens = 2048
openai_chat_providers = [{ name = "provider-b", model = "pb-model" }]
"#
    }

    fn test_config() -> Config {
        toml::from_str(test_config_toml()).expect("valid test config")
    }

    #[test]
    fn canonical_format_is_idempotent() {
        let cfg = test_config();
        let first = canonical_format(&cfg).expect("first format");
        let parsed: Config = toml::from_str(&first).expect("parse first");
        let second = canonical_format(&parsed).expect("second format");
        assert_eq!(first, second, "canonical format must be idempotent");
    }

    #[test]
    fn canonical_format_round_trips() {
        let cfg = test_config();
        let formatted = canonical_format(&cfg).expect("format");
        let parsed: Config = toml::from_str(&formatted).expect("parse formatted config");
        let orig_bytes = crate::config_render::semantic_config_bytes(&cfg).unwrap();
        let parsed_bytes = crate::config_render::semantic_config_bytes(&parsed).unwrap();
        assert_eq!(
            orig_bytes, parsed_bytes,
            "semantic content must survive round-trip"
        );
    }

    #[test]
    fn canonical_format_has_two_blank_lines_between_top_level_sections() {
        let cfg = test_config();
        let formatted = canonical_format(&cfg).expect("format");
        assert!(
            formatted.contains("\n\n\n[fallback]"),
            "expected 2 blank lines before [fallback], got:\n{formatted}"
        );
    }

    #[test]
    fn canonical_format_has_two_blank_lines_between_sibling_providers() {
        let cfg = test_config();
        let formatted = canonical_format(&cfg).expect("format");
        assert!(
            formatted.contains("\n\n\n[providers.provider-b]"),
            "expected 2 blank lines before sibling provider, got:\n{formatted}"
        );
    }

    #[test]
    fn canonical_format_has_one_blank_line_between_provider_subtables() {
        let cfg = test_config();
        let formatted = canonical_format(&cfg).expect("format");
        let lines: Vec<&str> = formatted.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("[providers.deepseek.openai_chat]") {
                assert!(
                    i >= 2 && lines[i - 1].is_empty() && !lines[i - 2].is_empty(),
                    "expected exactly 1 blank line before provider sub-table at line {i}, got:\n{formatted}"
                );
                return;
            }
        }
        panic!("did not find [providers.deepseek.openai_chat] in output:\n{formatted}");
    }
}
