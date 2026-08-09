use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};

pub type JsonObject = Map<String, Value>;

pub fn load_object(path: &Path) -> Result<JsonObject> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .with_context(|| format!("{} must contain a JSON object", path.display()))
}

pub fn write_object(path: &Path, obj: &JsonObject) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(obj).context("failed to serialize JSON")? + "\n";
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

pub fn ensure_object<'a>(root: &'a mut JsonObject, key: &str) -> Result<&'a mut JsonObject> {
    root.entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    object_value(root, key, key)
}

pub fn object_value<'a>(
    root: &'a mut JsonObject,
    key: &str,
    label: &str,
) -> Result<&'a mut JsonObject> {
    root.get_mut(key)
        .and_then(Value::as_object_mut)
        .with_context(|| format!("{label} must be a JSON object"))
}

pub fn set_value(root: &mut JsonObject, path: &[&str], value: Value) -> Result<()> {
    let Some((leaf, parents)) = path.split_last() else {
        return Ok(());
    };
    let mut current = root;
    for key in parents {
        current = ensure_object(current, key)?;
    }
    current.insert((*leaf).to_string(), value);
    Ok(())
}

pub fn to_value<T: Serialize>(value: T, label: &str) -> Result<Value> {
    serde_json::to_value(value).with_context(|| format!("failed to serialize {label}"))
}

pub fn replace_object_entries<F>(
    object: &mut JsonObject,
    is_managed: F,
    entries: impl IntoIterator<Item = (String, Value)>,
) where
    F: Fn(&str, &Value) -> bool,
{
    object.retain(|key, value| !is_managed(key, value));
    object.extend(entries);
}

pub fn replace_array_items<F>(
    object: &mut JsonObject,
    key: &str,
    is_managed: F,
    new_items: Vec<Value>,
) where
    F: Fn(&Value) -> bool,
{
    let existing = object
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut merged: Vec<Value> = existing
        .into_iter()
        .filter(|item| !is_managed(item))
        .collect();
    merged.extend(new_items);
    object.insert(key.to_string(), Value::Array(merged));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_value_creates_parent_objects() {
        let mut root = Map::new();
        set_value(
            &mut root,
            &["security", "auth", "selectedType"],
            json!("openai"),
        )
        .expect("set value");

        assert_eq!(root["security"]["auth"]["selectedType"], json!("openai"));
    }

    #[test]
    fn replace_array_items_preserves_unmanaged_items() {
        let mut root = Map::new();
        root.insert(
            "openai".to_string(),
            json!([
                {"id": "old", "envKey": "LLM_PROXY_API_KEY"},
                {"id": "custom"}
            ]),
        );

        replace_array_items(
            &mut root,
            "openai",
            |item| item.get("envKey").and_then(Value::as_str) == Some("LLM_PROXY_API_KEY"),
            vec![json!({"id": "new", "envKey": "LLM_PROXY_API_KEY"})],
        );

        assert_eq!(root["openai"].as_array().unwrap().len(), 2);
        assert_eq!(root["openai"][0]["id"], json!("custom"));
        assert_eq!(root["openai"][1]["id"], json!("new"));
    }
}
