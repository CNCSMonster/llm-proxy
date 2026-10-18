use std::collections::HashMap;
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
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    fs::write(&tmp, &data).with_context(|| format!("failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            path.display(),
            tmp.display()
        )
    })
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

/// Merge freshly generated `entries` into the object, deep-merging managed keys.
///
/// Similar to [`merge_managed_array_items`] but for objects instead of arrays.
/// Keys that match `is_managed` are deep-merged with the corresponding generated
/// entry (existing as base, generated as overlay). Non-managed keys are kept
/// verbatim. Managed keys with no generated counterpart are dropped.
pub fn merge_managed_object_entries<F>(
    object: &mut JsonObject,
    is_managed: F,
    entries: impl IntoIterator<Item = (String, Value)>,
) where
    F: Fn(&str) -> bool,
{
    let new_entries: Vec<(String, Value)> = entries.into_iter().collect();

    // Collect existing managed entries for deep-merge
    let mut managed_by_key: HashMap<String, Value> = HashMap::new();
    let mut preserved: Vec<(String, Value)> = Vec::new();

    for (key, value) in object.iter() {
        if is_managed(key) {
            managed_by_key.insert(key.clone(), value.clone());
        } else {
            preserved.push((key.clone(), value.clone()));
        }
    }

    // Build the merged result
    object.clear();
    object.extend(preserved);

    for (key, new_value) in new_entries {
        let final_value = match managed_by_key.remove(&key) {
            Some(old_value) => deep_merge(old_value, new_value),
            None => new_value,
        };
        object.insert(key, final_value);
    }
}

/// Recursively merge `overlay` on top of `base`.
///
/// Object values are merged key-by-key (overlay wins per key, base-only keys are
/// preserved); every other type — scalars and arrays alike — is taken from
/// `overlay` wholesale. This is the primitive behind model-level merge updates:
/// a client entry is the base, the freshly generated proxy entry is the overlay,
/// so proxy-owned fields win while client-only fields (e.g. `fastOnly`) and
/// nested client additions (e.g. `generationConfig.reasoning`) survive.
pub fn deep_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => overlay_value,
                };
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        (_, overlay) => overlay,
    }
}

/// Merge freshly generated `new_items` into the array stored at `key`.
///
/// Replaces the prior whole-entry overwrite, which silently dropped
/// client-only fields (e.g. `fastOnly`, nested `generationConfig.reasoning`) that
/// llm-proxy does not emit.
///
/// The set of items llm-proxy owns is authoritative: unmanaged items are kept
/// verbatim, managed items that still have a generated counterpart are merged by
/// `match_id` (existing entry as base, generated entry as overlay — see
/// [`deep_merge`]), and managed items with no counterpart are dropped (the model
/// was removed from the proxy). This preserves client-only fields on managed
/// entries while still letting the proxy own the fields it actually emits.
pub fn merge_managed_array_items<F, I>(
    object: &mut JsonObject,
    key: &str,
    is_managed: F,
    new_items: Vec<Value>,
    match_id: I,
) where
    F: Fn(&Value) -> bool,
    I: Fn(&Value) -> Option<&str>,
{
    let existing = object
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut preserved: Vec<Value> = Vec::new();
    let mut managed_by_id: HashMap<String, Value> = HashMap::new();
    for item in existing {
        if is_managed(&item) {
            if let Some(id) = match_id(&item) {
                managed_by_id.insert(id.to_string(), item);
            }
            // A managed item without an id cannot be matched to any generated
            // counterpart, so it is dropped (matches the previous replace set).
        } else {
            preserved.push(item);
        }
    }

    for new_item in new_items {
        let final_item = match match_id(&new_item).map(str::to_string) {
            Some(id) => match managed_by_id.remove(&id) {
                Some(old_item) => deep_merge(old_item, new_item),
                None => new_item,
            },
            None => new_item,
        };
        preserved.push(final_item);
    }

    object.insert(key.to_string(), Value::Array(preserved));
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

    fn is_managed(item: &Value) -> bool {
        item.get("envKey").and_then(Value::as_str) == Some("LLM_PROXY_API_KEY")
    }

    fn id_of(item: &Value) -> Option<&str> {
        item.get("id").and_then(Value::as_str)
    }

    #[test]
    fn merge_keeps_unmanaged_and_drops_managed_without_counterpart() {
        let mut root = Map::new();
        root.insert(
            "openai".to_string(),
            json!([
                {"id": "removed", "envKey": "LLM_PROXY_API_KEY"},
                {"id": "custom"}
            ]),
        );

        merge_managed_array_items(
            &mut root,
            "openai",
            is_managed,
            vec![json!({"id": "new", "envKey": "LLM_PROXY_API_KEY"})],
            id_of,
        );

        let arr = root["openai"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("custom"));
        assert_eq!(arr[1]["id"], json!("new"));
    }

    #[test]
    fn merge_preserves_client_fields_on_matched_managed_item() {
        let mut root = Map::new();
        root.insert(
            "openai".to_string(),
            json!([
                {
                    "id": "m",
                    "envKey": "LLM_PROXY_API_KEY",
                    "baseUrl": "http://stale/v1",
                    "fastOnly": true,
                    "generationConfig": {
                        "contextWindowSize": 1,
                        "reasoning": { "effort": "max" }
                    }
                }
            ]),
        );

        merge_managed_array_items(
            &mut root,
            "openai",
            is_managed,
            vec![json!({
                "id": "m",
                "envKey": "LLM_PROXY_API_KEY",
                "baseUrl": "http://proxy/v1",
                "generationConfig": { "contextWindowSize": 999, "maxOutputTokens": 42 }
            })],
            id_of,
        );

        let item = &root["openai"].as_array().unwrap()[0];
        // proxy-owned fields overwrite
        assert_eq!(item["baseUrl"], json!("http://proxy/v1"));
        assert_eq!(item["generationConfig"]["contextWindowSize"], json!(999));
        assert_eq!(item["generationConfig"]["maxOutputTokens"], json!(42));
        // client-only fields survive
        assert_eq!(item["fastOnly"], json!(true));
        assert_eq!(
            item["generationConfig"]["reasoning"]["effort"],
            json!("max")
        );
    }
}
