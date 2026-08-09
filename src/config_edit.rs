use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::DocumentMut;

use crate::config::Config;

pub fn write_full_config(path: &Path, cfg: &Config) -> Result<()> {
    cfg.validate()?;
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config")?;
    atomic_write(path, serialized.as_bytes())
}

pub fn write_provider(path: &Path, cfg: &Config, provider_id: &str) -> Result<()> {
    cfg.validate()?;
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config")?;
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
    write_doc(path, doc)
}

pub fn write_model(path: &Path, cfg: &Config, model_id: &str) -> Result<()> {
    cfg.validate()?;
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config")?;
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
    write_doc(path, doc)
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
    atomic_write(path, updated.as_bytes())
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
