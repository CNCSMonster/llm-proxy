use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use crate::config::Config;

/// 每次渲染的"我负责哪些节点"边界。不含增删改意图，增删由 next 与文档的差集推出。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum PatchScope<'a> {
    /// 整份文档由 next 权威（server writer 用）
    Whole,
    /// 只同步这些实体；其余节点一个字节都不动（standalone 实体写用）
    #[allow(dead_code)]
    Entities {
        providers: &'a [&'a str],
        models: &'a [&'a str],
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderError {
    /// 原文不是合法 TOML——必须拒绝，绝不回退全量写
    OriginalParse { detail: String },
    /// next 无法序列化为定义态 TOML（提交方数据问题；用于 400 预检）
    NextSerialize { detail: String },
    /// 定义态文本无法解析回 DocumentMut（内部不变量破裂，理论不可达）
    SourceParse { detail: String },
    /// I3 自检失败：渲染结果解析失败，或与 next 语义不等（S7）
    Verify { detail: String },
    /// patch 过程的结构性拒绝（保留位：类冲突时可选择拒绝而非重建）
    #[allow(dead_code)]
    Patch { path: String, detail: String },
}

impl RenderError {
    /// Q5 结构化日志的原因分类，取值稳定：`"original_parse" | "next_serialize"
    /// | "source_parse" | "verify" | "patch"`
    #[allow(dead_code)]
    pub(crate) fn reason_class(&self) -> &'static str {
        match self {
            Self::OriginalParse { .. } => "original_parse",
            Self::NextSerialize { .. } => "next_serialize",
            Self::SourceParse { .. } => "source_parse",
            Self::Verify { .. } => "verify",
            Self::Patch { .. } => "patch",
        }
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OriginalParse { detail } => {
                write!(f, "failed to parse original document: {detail}")
            }
            Self::NextSerialize { detail } => {
                write!(f, "failed to serialize next config: {detail}")
            }
            Self::SourceParse { detail } => {
                write!(f, "failed to parse definition-state document: {detail}")
            }
            Self::Verify { detail } => {
                write!(f, "rendered config verification failed: {detail}")
            }
            Self::Patch { path, detail } => {
                write!(f, "patch failed at '{path}': {detail}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

// Config uses BTreeMap for maps and serde struct field order is stable. The
// parsed Config has already materialized serde defaults, so this is the
// domain-normalized semantic payload rather than TOML presentation bytes.
pub(crate) fn semantic_config_bytes(config: &Config) -> Result<Vec<u8>, String> {
    serde_json::to_vec(config).map_err(|error| error.to_string())
}

#[allow(dead_code)]
fn definition_state(next: &Config) -> Result<DocumentMut, RenderError> {
    let toml_str = toml::to_string_pretty(next).map_err(|e| RenderError::NextSerialize {
        detail: e.to_string(),
    })?;
    toml_str
        .parse::<DocumentMut>()
        .map_err(|e| RenderError::SourceParse {
            detail: e.to_string(),
        })
}

/// 无原文时的定义态渲染（文件不存在 / 首次生成）。成功即可直接落盘。
#[allow(dead_code)]
pub(crate) fn render_new(next: &Config) -> Result<String, RenderError> {
    let doc = definition_state(next)?;
    let rendered = doc.to_string();
    verify(&rendered, next, PatchScope::Whole, "")?;
    Ok(rendered)
}

#[allow(dead_code)]
fn expected_config_for_scope(
    original: &str,
    next: &Config,
    providers: &[&str],
    models: &[&str],
) -> Result<Config, RenderError> {
    let mut expected: Config = toml::from_str(original).map_err(|e| RenderError::Verify {
        detail: format!("failed to parse original for scope verification: {e}"),
    })?;
    for &p in providers {
        if let Some(pv) = next.providers.get(p) {
            expected.providers.insert(p.to_string(), pv.clone());
        } else {
            expected.providers.remove(p);
        }
    }
    for &m in models {
        if let Some(mv) = next.models.get(m) {
            expected.models.insert(m.to_string(), mv.clone());
        } else {
            expected.models.remove(m);
        }
    }
    Ok(expected)
}

#[allow(dead_code)]
fn verify(
    rendered: &str,
    next: &Config,
    scope: PatchScope<'_>,
    original: &str,
) -> Result<(), RenderError> {
    let parsed: Config = toml::from_str(rendered).map_err(|e| RenderError::Verify {
        detail: e.to_string(),
    })?;
    let expected_holder;
    let expected: &Config = match scope {
        PatchScope::Whole => next,
        PatchScope::Entities { providers, models } => {
            expected_holder = expected_config_for_scope(original, next, providers, models)?;
            &expected_holder
        }
    };
    let parsed_bytes =
        semantic_config_bytes(&parsed).map_err(|e| RenderError::Verify { detail: e })?;
    let expected_bytes =
        semantic_config_bytes(expected).map_err(|e| RenderError::Verify { detail: e })?;
    if parsed_bytes != expected_bytes {
        return Err(RenderError::Verify {
            detail: "rendered config is not semantically equal to expected config".into(),
        });
    }
    Ok(())
}

fn clone_table_position_free(table: &Table) -> Table {
    let mut new_table = Table::new();
    *new_table.decor_mut() = table.decor().clone();
    new_table.set_implicit(table.is_implicit());
    new_table.set_dotted(table.is_dotted());
    for (key, item) in table.iter() {
        new_table.insert(key, clone_position_free(item));
    }
    new_table
}

fn clone_position_free(item: &Item) -> Item {
    match item {
        Item::Table(t) => Item::Table(clone_table_position_free(t)),
        Item::ArrayOfTables(a) => {
            let mut new_a = ArrayOfTables::new();
            for t in a.iter() {
                new_a.push(clone_table_position_free(t));
            }
            Item::ArrayOfTables(new_a)
        }
        Item::Value(v) => Item::Value(v.clone()),
        Item::None => Item::None,
    }
}

fn int_float_equal(int_val: i64, float_val: f64) -> bool {
    if !float_val.is_finite() || float_val.fract() != 0.0 {
        return false;
    }
    if float_val < i64::MIN as f64 || float_val > i64::MAX as f64 {
        return false;
    }
    (int_val as f64 == float_val) && (float_val as i64 == int_val)
}

fn values_semantically_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(s1), Value::String(s2)) => s1.value() == s2.value(),
        (Value::Integer(i1), Value::Integer(i2)) => i1.value() == i2.value(),
        (Value::Float(f1), Value::Float(f2)) => {
            let v1 = *f1.value();
            let v2 = *f2.value();
            if v1.is_nan() && v2.is_nan() {
                true
            } else {
                v1 == v2
            }
        }
        (Value::Boolean(b1), Value::Boolean(b2)) => b1.value() == b2.value(),
        (Value::Datetime(d1), Value::Datetime(d2)) => d1.value() == d2.value(),
        (Value::Integer(i), Value::Float(f)) => int_float_equal(*i.value(), *f.value()),
        (Value::Float(f), Value::Integer(i)) => int_float_equal(*i.value(), *f.value()),
        _ => false,
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::InlineTable(_))
}

fn patch_scalar(existing: &mut Value, source: &Value) {
    if values_semantically_equal(existing, source) {
        return;
    }
    let old_decor = existing.decor().clone();
    let mut fresh = source.clone();
    *fresh.decor_mut() = old_decor;
    *existing = fresh;
}

fn patch_plain_array(
    existing: &mut toml_edit::Array,
    source: &toml_edit::Array,
    path: &str,
) -> Result<(), RenderError> {
    let common_len = existing.len().min(source.len());
    for i in 0..common_len {
        let existing_val = existing.get_mut(i).expect("i < common_len");
        let source_val = source.get(i).expect("i < common_len");
        let elem_path = format!("{path}[{i}]");
        if is_scalar(existing_val) && is_scalar(source_val) {
            patch_scalar(existing_val, source_val);
        } else if existing_val.is_inline_table() && source_val.is_inline_table() {
            patch_table_like(
                existing_val.as_inline_table_mut().unwrap(),
                source_val.as_inline_table().unwrap(),
                None,
                &elem_path,
            )?;
        } else if existing_val.is_array() && source_val.is_array() {
            patch_plain_array(
                existing_val.as_array_mut().unwrap(),
                source_val.as_array().unwrap(),
                &elem_path,
            )?;
        } else {
            *existing_val = source_val.clone();
        }
    }
    if source.len() > existing.len() {
        for i in existing.len()..source.len() {
            let item = source.get(i).expect("i < source.len()").clone();
            existing.push_formatted(item);
        }
    } else if existing.len() > source.len() {
        while existing.len() > source.len() {
            existing.remove(existing.len() - 1);
        }
    }
    Ok(())
}

fn aot_element_identity(table: &Table) -> Option<&str> {
    table.get("name").and_then(|item| item.as_str())
}

fn has_duplicate_or_missing_identities(identities: &[Option<&str>]) -> bool {
    let mut seen = std::collections::HashSet::new();
    for id in identities {
        match id {
            Some(name) => {
                if !seen.insert(*name) {
                    return true;
                }
            }
            None => return true,
        }
    }
    false
}

fn patch_array_of_tables(
    existing: &mut ArrayOfTables,
    source: &ArrayOfTables,
    path: &str,
) -> Result<(), RenderError> {
    let existing_identities: Vec<Option<&str>> =
        existing.iter().map(aot_element_identity).collect();
    let source_identities: Vec<Option<&str>> = source.iter().map(aot_element_identity).collect();

    if has_duplicate_or_missing_identities(&existing_identities)
        || has_duplicate_or_missing_identities(&source_identities)
    {
        return Ok(());
    }

    let source_names: std::collections::HashSet<&str> =
        source_identities.iter().copied().flatten().collect();

    // 1. Remove elements absent in source
    let mut i = 0;
    while i < existing.len() {
        let name = aot_element_identity(existing.get(i).unwrap());
        if name.is_some_and(|n| !source_names.contains(n)) {
            existing.remove(i);
            continue;
        }
        i += 1;
    }

    // 2. In-place patch matched elements
    for (src_idx, source_table) in source.iter().enumerate() {
        let src_name = aot_element_identity(source_table).expect("identities verified unique");
        let exist_idx = existing
            .iter()
            .position(|t| aot_element_identity(t) == Some(src_name));
        if let Some(exist_idx) = exist_idx {
            let elem_path = format!("{path}[{src_idx}]");
            let existing_table = existing.get_mut(exist_idx).unwrap();
            patch_table_like(existing_table, source_table, None, &elem_path)?;
        }
    }

    // 3. Append new elements to array tail
    for source_table in source.iter() {
        let src_name = aot_element_identity(source_table).expect("identities verified unique");
        let exists = existing
            .iter()
            .any(|t| aot_element_identity(t) == Some(src_name));
        if !exists {
            existing.push(clone_table_position_free(source_table));
        }
    }

    Ok(())
}

fn patch_item(existing: &mut Item, source: &Item, path: &str) -> Result<(), RenderError> {
    if existing.is_table_like() && source.is_table_like() {
        let existing_table = existing.as_table_like_mut().expect("is_table_like checked");
        let source_table = source.as_table_like().expect("is_table_like checked");
        patch_table_like(existing_table, source_table, None, path)?;
    } else if let (Some(existing_val), Some(source_val)) =
        (existing.as_value_mut(), source.as_value())
    {
        if is_scalar(existing_val) && is_scalar(source_val) {
            patch_scalar(existing_val, source_val);
        } else if let (Some(existing_arr), Some(source_arr)) =
            (existing_val.as_array_mut(), source_val.as_array())
        {
            patch_plain_array(existing_arr, source_arr, path)?;
        } else {
            // Class conflict: rebuild
            *existing = clone_position_free(source);
        }
    } else if let (Some(existing_aot), Some(source_aot)) = (
        existing.as_array_of_tables_mut(),
        source.as_array_of_tables(),
    ) {
        patch_array_of_tables(existing_aot, source_aot, path)?;
    } else {
        // Class conflict (e.g. table vs scalar): rebuild
        *existing = clone_position_free(source);
    }
    Ok(())
}

fn allows_additions(path: &str, key: &str) -> bool {
    if path.is_empty() {
        // At root: only providers and models can be established when absent
        key == "providers" || key == "models"
    } else if path == "server" {
        // In server: don't inject default fields that user didn't write
        false
    } else {
        // In providers, models, providers.<id>, models.<id>, etc.
        true
    }
}

fn patch_table_like(
    existing: &mut dyn TableLike,
    source: &dyn TableLike,
    keys: Option<&[&str]>,
    path: &str,
) -> Result<(), RenderError> {
    if let Some(scope_keys) = keys {
        for &key in scope_keys {
            let child_path = if path.is_empty() {
                key.to_string()
            } else {
                format!("{path}.{key}")
            };
            if let Some(source_item) = source.get(key) {
                let slot = existing
                    .entry(key)
                    .or_insert_with(|| clone_position_free(source_item));
                patch_item(slot, source_item, &child_path)?;
            } else if existing.contains_key(key) {
                existing.remove(key);
            }
        }
    } else {
        for (key, source_item) in source.iter() {
            if !existing.contains_key(key) && !allows_additions(path, key) {
                continue;
            }
            if path.is_empty()
                && (key == "providers" || key == "models")
                && source_item
                    .as_table_like()
                    .is_some_and(|t| t.is_empty() && !existing.contains_key(key))
            {
                continue;
            }
            let child_path = if path.is_empty() {
                key.to_string()
            } else {
                format!("{path}.{key}")
            };
            let slot = existing
                .entry(key)
                .or_insert_with(|| clone_position_free(source_item));
            patch_item(slot, source_item, &child_path)?;
        }

        let to_remove: Vec<String> = existing
            .iter()
            .map(|(k, _)| k.to_string())
            .filter(|k| !source.contains_key(k.as_str()))
            .collect();
        for key in to_remove {
            existing.remove(&key);
        }
    }
    Ok(())
}

fn patch_root(
    doc: &mut DocumentMut,
    source: &DocumentMut,
    scope: PatchScope<'_>,
) -> Result<(), RenderError> {
    match scope {
        PatchScope::Whole => patch_table_like(doc.as_table_mut(), source.as_table(), None, ""),
        PatchScope::Entities { providers, models } => {
            if !providers.is_empty() {
                let doc_has = doc.as_table().contains_key("providers");
                let src_item = source.get("providers");
                if doc_has || src_item.is_some() {
                    let slot = doc.as_table_mut().entry("providers").or_insert_with(|| {
                        let mut t = Table::new();
                        if let Some(st) = src_item.and_then(|i| i.as_table()) {
                            t.set_implicit(st.is_implicit());
                        } else {
                            t.set_implicit(true);
                        }
                        Item::Table(t)
                    });
                    if let Some(existing_table) = slot.as_table_like_mut() {
                        let empty_table = Table::new();
                        let src_table = src_item
                            .and_then(|i| i.as_table_like())
                            .unwrap_or(&empty_table);
                        patch_table_like(existing_table, src_table, Some(providers), "providers")?;
                    }
                }
            }

            if !models.is_empty() {
                let doc_has = doc.as_table().contains_key("models");
                let src_item = source.get("models");
                if doc_has || src_item.is_some() {
                    let slot = doc.as_table_mut().entry("models").or_insert_with(|| {
                        let mut t = Table::new();
                        if let Some(st) = src_item.and_then(|i| i.as_table()) {
                            t.set_implicit(st.is_implicit());
                        } else {
                            t.set_implicit(true);
                        }
                        Item::Table(t)
                    });
                    if let Some(existing_table) = slot.as_table_like_mut() {
                        let empty_table = Table::new();
                        let src_table = src_item
                            .and_then(|i| i.as_table_like())
                            .unwrap_or(&empty_table);
                        patch_table_like(existing_table, src_table, Some(models), "models")?;
                    }
                }
            }

            Ok(())
        }
    }
}

/// 增量渲染：presentation 由 original 决定，值由 next 决定。
#[allow(dead_code)]
pub(crate) fn render_with_comments(
    original: &str,
    next: &Config,
    scope: PatchScope<'_>,
) -> Result<String, RenderError> {
    let mut doc = original
        .parse::<DocumentMut>()
        .map_err(|e| RenderError::OriginalParse {
            detail: e.to_string(),
        })?;
    let source = definition_state(next)?;
    patch_root(&mut doc, &source, scope)?;
    let rendered = doc.to_string();
    verify(&rendered, next, scope, original)?;
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
# 行前注释：绑定顺序敏感
[[models.m1.openai_chat_providers]]
# 元素注释
name = "p1"
model = "upstream-a"
"#;

    #[test]
    fn r1_semantic_equal_yields_exact_same_bytes() {
        let next: Config = toml::from_str(BASE).expect("BASE must be valid Config");
        let rendered = render_with_comments(BASE, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed for semantically equal config");
        assert_eq!(rendered, BASE);
    }

    #[test]
    fn r2_modify_scalar_preserves_leading_and_trailing_comments() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
# before: context
context_window = 4096  # trailing
max_output_tokens = 1024
# 行前注释：绑定顺序敏感
[[models.m1.openai_chat_providers]]
# 元素注释
name = "p1"
model = "upstream-a"
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.models.get_mut("m1").unwrap().context_window = 8192;
        let rendered = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed");
        assert!(
            rendered.contains("# before: context"),
            "leading comment must survive"
        );
        assert!(
            rendered.contains("# trailing"),
            "trailing comment must survive"
        );
        assert!(
            rendered.contains("context_window = 8192"),
            "value must be updated"
        );
        let providers_section = "[providers.p1]\nproduct = \"custom\"\napi_key_env = \"P1_KEY\"";
        assert!(
            rendered.contains(providers_section),
            "providers section bytes must remain unchanged"
        );
    }

    #[test]
    fn r6_idempotent_rendering_yields_exact_same_bytes() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
# before: context
context_window = 4096  # trailing
max_output_tokens = 1024
# 行前注释：绑定顺序敏感
[[models.m1.openai_chat_providers]]
# 元素注释
name = "p1"
model = "upstream-a"
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.models.get_mut("m1").unwrap().context_window = 8192;
        let first = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("first render must succeed");
        let second = render_with_comments(&first, &next, PatchScope::Whole)
            .expect("second render must succeed");
        assert_eq!(
            first, second,
            "consecutive renders must produce exact same bytes"
        );
    }

    #[test]
    fn r8_unparseable_original_is_rejected() {
        let fixture = "[providers.alpha\nlisten = 1";
        let next: Config = toml::from_str(BASE).expect("BASE must be valid Config");
        let err = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect_err("unparseable original must be rejected");
        match err {
            RenderError::OriginalParse { .. } => {}
            other => panic!("expected RenderError::OriginalParse, got: {other:?}"),
        }
    }

    #[test]
    fn r10_f64_integer_literal_roundtrip_semantic_equality() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[server.usage]
file_threshold_mb = 2
migration_ratio = 1
db_max_size_mb = 50.0

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
[[models.m1.openai_chat_providers]]
name = "p1"
model = "upstream-a"
"#;
        // Case 1: fixture with integer literals, semantic equal next
        let next_same: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        let rendered_same = render_with_comments(fixture, &next_same, PatchScope::Whole)
            .expect("render_with_comments must succeed for semantic equal config");
        assert_eq!(
            rendered_same, fixture,
            "integer literals must not be rewritten when semantically equal"
        );

        // Case 2: modify another field in the same section to trigger write path
        let mut next_mod = next_same.clone();
        next_mod.server.as_mut().unwrap().usage.db_max_size_mb = 100.0;
        let rendered_mod1 = render_with_comments(fixture, &next_mod, PatchScope::Whole)
            .expect("first modified render must succeed");
        assert!(
            rendered_mod1.contains("file_threshold_mb = 2"),
            "file_threshold_mb integer literal must remain intact"
        );
        assert!(
            rendered_mod1.contains("migration_ratio = 1"),
            "migration_ratio integer literal must remain intact"
        );
        assert!(
            rendered_mod1.contains("db_max_size_mb = 100.0"),
            "db_max_size_mb must be updated"
        );

        let rendered_mod2 = render_with_comments(&rendered_mod1, &next_mod, PatchScope::Whole)
            .expect("second modified render must succeed");
        assert_eq!(rendered_mod1, rendered_mod2, "idempotent on write path");
    }

    #[test]
    fn r3_add_provider_preserves_existing_bytes_and_definition_state() {
        let mut next: Config = toml::from_str(BASE).expect("BASE must be valid Config");
        next.providers.insert(
            "p2".to_string(),
            crate::config::ProviderConfig {
                product: "custom".to_string(),
                api_key_env: Some("P2_KEY".to_string()),
                ..Default::default()
            },
        );
        let rendered = render_with_comments(BASE, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed");
        assert!(
            rendered.starts_with("# root 手写注释\n"),
            "root comment must remain byte-exact"
        );
        let p1_section = "[providers.p1]\nproduct = \"custom\"\napi_key_env = \"P1_KEY\"";
        assert!(
            rendered.contains(p1_section),
            "p1 section bytes must remain unchanged"
        );
        assert!(
            rendered.contains("[providers.p2]"),
            "p2 section must appear in definition state"
        );
        assert!(
            rendered.contains("api_key_env = \"P2_KEY\""),
            "p2 fields must be present"
        );
        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse to Config");
        assert_eq!(
            semantic_config_bytes(&parsed).unwrap(),
            semantic_config_bytes(&next).unwrap(),
            "rendered must be semantically equal to next"
        );
    }

    #[test]
    fn r4_delete_provider_fixture_pinned() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

# p1 section
[providers.p1]
# inside p1
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.providers.remove("p1");
        let rendered = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed");
        eprintln!("--- R4 rendered ---\n{rendered}");

        // 实测记录（2026-09-17）：TableLike::remove("p1") 移除了 p1 槽位，
        // 其表头前注释（# p1 section）和内部字段注释（# inside p1）随该节点一同消失。
        assert!(
            !rendered.contains("# p1 section"),
            "R4: 记录于 2026-09-17，删除 provider 后其表头前注释消失"
        );
        assert!(
            !rendered.contains("# inside p1"),
            "R4: 记录于 2026-09-17，删除 provider 后其内部注释消失"
        );
        assert!(
            !rendered.contains("providers.p1"),
            "p1 section must be removed"
        );
        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse to Config");
        assert_eq!(
            semantic_config_bytes(&parsed).unwrap(),
            semantic_config_bytes(&next).unwrap(),
            "I3: rendered config must be semantically equal to next"
        );
    }

    #[test]
    fn r11_features_insert_shifts_following_indices_fixture_pinned() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
features = ["a", "b"]
[[models.m1.openai_chat_providers]]
name = "p1"
model = "upstream-a"
"#;
        // Subcase 1: insert "c" in the middle -> ["a", "c", "b"]
        let mut next_mid: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next_mid.models.get_mut("m1").unwrap().features = vec!["a".into(), "c".into(), "b".into()];
        let rendered_mid = render_with_comments(fixture, &next_mid, PatchScope::Whole)
            .expect("middle insert must succeed");
        eprintln!("--- R11 middle insert ---\n{rendered_mid}");
        let parsed_mid: Config =
            toml::from_str(&rendered_mid).expect("rendered_mid must parse to Config");
        assert_eq!(
            parsed_mid.models["m1"].features,
            vec!["a", "c", "b"],
            "features sequence must match next in middle insert"
        );
        assert_eq!(
            semantic_config_bytes(&parsed_mid).unwrap(),
            semantic_config_bytes(&next_mid).unwrap(),
            "I3: middle insert must be semantically equal"
        );

        // Subcase 2: append "c" at tail -> ["a", "b", "c"]
        let mut next_tail: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next_tail.models.get_mut("m1").unwrap().features = vec!["a".into(), "b".into(), "c".into()];
        let rendered_tail = render_with_comments(fixture, &next_tail, PatchScope::Whole)
            .expect("tail append must succeed");
        eprintln!("--- R11 tail append ---\n{rendered_tail}");
        let parsed_tail: Config =
            toml::from_str(&rendered_tail).expect("rendered_tail must parse to Config");
        assert_eq!(
            parsed_tail.models["m1"].features,
            vec!["a", "b", "c"],
            "features sequence must match next in tail append"
        );
        assert_eq!(
            semantic_config_bytes(&parsed_tail).unwrap(),
            semantic_config_bytes(&next_tail).unwrap(),
            "I3: tail append must be semantically equal"
        );
    }

    #[test]
    fn r12_scope_entities_prevents_unauthorized_mutation() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

# p2 keep
[providers.p2]
product = "custom"
api_key_env = "P2_ORIGINAL_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
[[models.m1.openai_chat_providers]]
name = "p1"
model = "upstream-a"
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.providers.get_mut("p1").unwrap().api_key_env = Some("P1_MODIFIED".into());
        next.providers.get_mut("p2").unwrap().api_key_env = Some("P2_DIFFERS_IN_NEXT".into());

        let rendered = render_with_comments(
            fixture,
            &next,
            PatchScope::Entities {
                providers: &["p1"],
                models: &[],
            },
        )
        .expect("render_with_comments with Entities scope must succeed");

        assert!(
            rendered.contains("api_key_env = \"P1_MODIFIED\""),
            "p1 in scope must be updated"
        );
        let p2_section =
            "# p2 keep\n[providers.p2]\nproduct = \"custom\"\napi_key_env = \"P2_ORIGINAL_KEY\"";
        assert!(
            rendered.contains(p2_section),
            "p2 out of scope must remain byte-exact including comments"
        );
        assert!(
            !rendered.contains("P2_DIFFERS_IN_NEXT"),
            "p2 mutation in next must not leak into rendered output"
        );
        let root_section = "# root 手写注释\n[server]\nlisten = \"127.0.0.1:8992\"";
        assert!(
            rendered.contains(root_section),
            "root section must remain unchanged"
        );

        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse to Config");
        assert_eq!(
            parsed.providers["p1"].api_key_env.as_deref(),
            Some("P1_MODIFIED"),
            "p1 should have modified value"
        );
        assert_eq!(
            parsed.providers["p2"].api_key_env.as_deref(),
            Some("P2_ORIGINAL_KEY"),
            "p2 should retain original value"
        );
    }

    #[test]
    fn r3_missing_tables_established_in_definition_state() {
        let fixture = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.providers.insert(
            "p1".to_string(),
            crate::config::ProviderConfig {
                product: "custom".to_string(),
                api_key_env: Some("P1_KEY".to_string()),
                ..Default::default()
            },
        );
        let rendered = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed");
        assert!(
            rendered.starts_with("# root 手写注释\n"),
            "root comment must remain byte-exact"
        );
        assert!(
            rendered.contains("[providers.p1]"),
            "providers table should be established in definition state"
        );
        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse to Config");
        assert_eq!(
            semantic_config_bytes(&parsed).unwrap(),
            semantic_config_bytes(&next).unwrap(),
            "I3: rendered config must be semantically equal to next"
        );
    }

    fn assert_r9_property(
        original: &str,
        next: &Config,
        scope: PatchScope<'_>,
        case_name: &str,
    ) -> Result<String, RenderError> {
        let result = render_with_comments(original, next, scope);
        match &result {
            Ok(rendered) => {
                let parsed: Config = match toml::from_str(rendered) {
                    Ok(c) => c,
                    Err(e) => {
                        panic!(
                            "[{case_name}] I3 violation: rendered config failed to parse into Config: {e}\nRendered:\n{rendered}"
                        );
                    }
                };
                let expected_holder;
                let expected: &Config = match scope {
                    PatchScope::Whole => next,
                    PatchScope::Entities { providers, models } => {
                        match expected_config_for_scope(original, next, providers, models) {
                            Ok(c) => {
                                expected_holder = c;
                                &expected_holder
                            }
                            Err(e) => {
                                panic!(
                                    "[{case_name}] I3 violation: render succeeded but expected_config_for_scope failed: {e:?}"
                                );
                            }
                        }
                    }
                };
                let parsed_bytes =
                    semantic_config_bytes(&parsed).expect("semantic bytes of parsed");
                let expected_bytes =
                    semantic_config_bytes(expected).expect("semantic bytes of expected");
                assert_eq!(
                    parsed_bytes, expected_bytes,
                    "[{case_name}] I3 violation: SILENT MIS-WRITE! Rendered config is not semantically equal to expected!"
                );
            }
            Err(err) => {
                eprintln!("[{case_name}] Err returned: {err:?}");
            }
        }
        result
    }

    #[test]
    fn r9_adversarial_1_dotted_key() {
        let fixture = r#"# root comment
server.listen = "127.0.0.1:8992"

providers.p1.product = "custom"
providers.p1.api_key_env = "P1_KEY"

models.m1.context_window = 4096
models.m1.max_output_tokens = 1024
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.models.get_mut("m1").unwrap().max_output_tokens = 2048;

        let res_whole =
            assert_r9_property(fixture, &next, PatchScope::Whole, "R9.1 dotted key (Whole)");
        let res_entities = assert_r9_property(
            fixture,
            &next,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.1 dotted key (Entities)",
        );
        eprintln!(
            "R9.1 outcomes: Whole={:?}, Entities={:?}",
            res_whole.as_ref().map(|_| "Ok"),
            res_entities.as_ref().map(|_| "Ok")
        );
    }

    #[test]
    fn r9_adversarial_2_inline_table() {
        let fixture = r#"# root comment
[server]
listen = "127.0.0.1:8992"

[providers]
p1 = { product = "custom", api_key_env = "P1_KEY" }

[models]
m1 = { context_window = 4096, max_output_tokens = 1024 }
"#;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        next.providers.get_mut("p1").unwrap().api_key_env = Some("P1_MODIFIED".into());
        next.models.get_mut("m1").unwrap().max_output_tokens = 2048;

        let res_whole = assert_r9_property(
            fixture,
            &next,
            PatchScope::Whole,
            "R9.2 inline table (Whole)",
        );
        let res_entities = assert_r9_property(
            fixture,
            &next,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.2 inline table (Entities)",
        );
        eprintln!(
            "R9.2 outcomes: Whole={:?}, Entities={:?}",
            res_whole.as_ref().map(|_| "Ok"),
            res_entities.as_ref().map(|_| "Ok")
        );
    }

    #[test]
    fn r9_adversarial_3_binding_array_as_inline_tables() {
        let fixture = r#"# root comment
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
openai_chat_providers = [{ name = "p1", model = "upstream-a" }]
"#;
        // Subcase 3a: next modifies scalar max_output_tokens (binding remains semantically same)
        let mut next_scalar: Config =
            toml::from_str(fixture).expect("fixture must be valid Config");
        next_scalar.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_3a_whole = assert_r9_property(
            fixture,
            &next_scalar,
            PatchScope::Whole,
            "R9.3 inline table binding array - scalar mod (Whole)",
        );
        let res_3a_entities = assert_r9_property(
            fixture,
            &next_scalar,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.3 inline table binding array - scalar mod (Entities)",
        );

        // Subcase 3b: next modifies binding itself (model changed)
        let mut next_bind = next_scalar.clone();
        next_bind
            .models
            .get_mut("m1")
            .unwrap()
            .openai_chat_providers[0]
            .model = "upstream-b".into();
        let res_3b_whole = assert_r9_property(
            fixture,
            &next_bind,
            PatchScope::Whole,
            "R9.3 inline table binding array - binding mod (Whole)",
        );
        let res_3b_entities = assert_r9_property(
            fixture,
            &next_bind,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.3 inline table binding array - binding mod (Entities)",
        );
        eprintln!(
            "R9.3 outcomes: 3a_Whole={:?}, 3a_Entities={:?}, 3b_Whole={:?}, 3b_Entities={:?}",
            res_3a_whole.as_ref().map(|_| "Ok"),
            res_3a_entities.as_ref().map(|_| "Ok"),
            res_3b_whole.as_ref().map(|_| "Ok"),
            res_3b_entities.as_ref().map(|_| "Ok")
        );
    }

    #[test]
    fn r9_adversarial_4_type_switch_conflicts() {
        // 4a: scalar vs table (context_window was written as a table in original)
        let fixture_scalar_table = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = { invalid_table = 4096 }
max_output_tokens = 1024
"#;
        let mut next: Config = toml::from_str(BASE).expect("BASE must be valid Config");
        next.models.get_mut("m1").unwrap().context_window = 4096;
        next.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_4a_whole = assert_r9_property(
            fixture_scalar_table,
            &next,
            PatchScope::Whole,
            "R9.4a scalar-to-table conflict (Whole)",
        );
        let res_4a_entities = assert_r9_property(
            fixture_scalar_table,
            &next,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.4a scalar-to-table conflict (Entities)",
        );

        // 4b: plain array vs table (features was a table in original)
        let fixture_array_table = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
features = { not_an_array = true }
"#;
        let mut next_features = next.clone();
        next_features.models.get_mut("m1").unwrap().features =
            vec!["feat_a".into(), "feat_b".into()];
        let res_4b_whole = assert_r9_property(
            fixture_array_table,
            &next_features,
            PatchScope::Whole,
            "R9.4b array-to-table conflict (Whole)",
        );
        let res_4b_entities = assert_r9_property(
            fixture_array_table,
            &next_features,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.4b array-to-table conflict (Entities)",
        );

        // 4c: scalar vs plain array (features was scalar in original)
        let fixture_array_scalar = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
features = "scalar_features"
"#;
        let res_4c_whole = assert_r9_property(
            fixture_array_scalar,
            &next_features,
            PatchScope::Whole,
            "R9.4c array-to-scalar conflict (Whole)",
        );
        let res_4c_entities = assert_r9_property(
            fixture_array_scalar,
            &next_features,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.4c array-to-scalar conflict (Entities)",
        );

        eprintln!(
            "R9.4 outcomes: 4a_Whole={:?}, 4a_Entities={:?}, 4b_Whole={:?}, 4b_Entities={:?}, 4c_Whole={:?}, 4c_Entities={:?}",
            res_4a_whole.as_ref().map(|_| "Ok"),
            res_4a_entities.as_ref().map(|_| "Ok"),
            res_4b_whole.as_ref().map(|_| "Ok"),
            res_4b_entities.as_ref().map(|_| "Ok"),
            res_4c_whole.as_ref().map(|_| "Ok"),
            res_4c_entities.as_ref().map(|_| "Ok")
        );
    }

    #[test]
    fn r9_adversarial_5_duplicate_elements() {
        // 5a: duplicate elements in plain array
        let fixture_array_dup = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
features = ["dup", "dup", "other"]
"#;
        let mut next_dup: Config =
            toml::from_str(fixture_array_dup).expect("fixture_array_dup must be valid Config");
        next_dup.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_5a_whole = assert_r9_property(
            fixture_array_dup,
            &next_dup,
            PatchScope::Whole,
            "R9.5a duplicate plain array elements - keep (Whole)",
        );
        let res_5a_entities = assert_r9_property(
            fixture_array_dup,
            &next_dup,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.5a duplicate plain array elements - keep (Entities)",
        );

        // 5a-2: next dedupes the plain array
        let mut next_dedup = next_dup.clone();
        next_dedup.models.get_mut("m1").unwrap().features = vec!["dup".into(), "other".into()];
        let res_5a2_whole = assert_r9_property(
            fixture_array_dup,
            &next_dedup,
            PatchScope::Whole,
            "R9.5a duplicate plain array elements - deduplicate (Whole)",
        );

        // 5b: duplicate elements in AoT
        let fixture_aot_dup = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024

[[models.m1.openai_chat_providers]]
name = "p1"
model = "upstream-a"

[[models.m1.openai_chat_providers]]
name = "p1"
model = "upstream-a"
"#;
        let mut next_aot_dup: Config =
            toml::from_str(fixture_aot_dup).expect("fixture_aot_dup must be valid Config");
        next_aot_dup.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_5b_whole = assert_r9_property(
            fixture_aot_dup,
            &next_aot_dup,
            PatchScope::Whole,
            "R9.5b duplicate AoT elements (Whole)",
        );
        let res_5b_entities = assert_r9_property(
            fixture_aot_dup,
            &next_aot_dup,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.5b duplicate AoT elements (Entities)",
        );

        // 5c: duplicate table key in same table (invalid TOML syntax)
        let fixture_dup_key = r#"[server]
listen = "127.0.0.1:8992"
listen = "127.0.0.1:8993"
"#;
        let next_base: Config = toml::from_str(BASE).expect("BASE is valid");
        let res_5c = assert_r9_property(
            fixture_dup_key,
            &next_base,
            PatchScope::Whole,
            "R9.5c duplicate table key (Whole)",
        );
        assert!(matches!(res_5c, Err(RenderError::OriginalParse { .. })));

        // 5d: AoT element modification before S4/S5: I3 safety net active rejection
        let mut next_aot_mod = next_aot_dup.clone();
        next_aot_mod
            .models
            .get_mut("m1")
            .unwrap()
            .openai_chat_providers[0]
            .model = "upstream-b".into();
        let res_5d = assert_r9_property(
            fixture_aot_dup,
            &next_aot_mod,
            PatchScope::Whole,
            "R9.5d AoT element modification before S4 (Whole)",
        );
        assert!(
            matches!(res_5d, Err(RenderError::Verify { .. })),
            "I3 must reject unhandled AoT modifications before S4 rather than silently outputting wrong semantics"
        );

        eprintln!(
            "R9.5 outcomes: 5a_Whole={:?}, 5a_Entities={:?}, 5a2_Whole={:?}, 5b_Whole={:?}, 5b_Entities={:?}, 5c={:?}, 5d={:?}",
            res_5a_whole.as_ref().map(|_| "Ok"),
            res_5a_entities.as_ref().map(|_| "Ok"),
            res_5a2_whole.as_ref().map(|_| "Ok"),
            res_5b_whole.as_ref().map(|_| "Ok"),
            res_5b_entities.as_ref().map(|_| "Ok"),
            res_5c.as_ref().map(|_| "Ok"),
            res_5d.as_ref().map(|_| "Ok"),
        );
    }

    #[test]
    fn r9_adversarial_6_unknown_keys_fixture_pinned() {
        // 6a: unknown key at root — Config has deny_unknown_fields, so parse must fail
        let fixture_root = r#"unknown_root_key = "extra_root_value"

[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
"#;
        let res_root_parse: Result<Config, _> = toml::from_str(fixture_root);
        assert!(
            res_root_parse.is_err(),
            "R9.6a unknown root key must be rejected by deny_unknown_fields"
        );

        // 6b: unknown key in provider (ProviderConfig has deny_unknown_fields)
        let fixture_prov = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"
unknown_provider_key = "extra_prov_value"

[models.m1]
context_window = 4096
max_output_tokens = 1024
"#;
        let mut next_prov: Config = toml::from_str(BASE).expect("BASE is valid Config");
        next_prov.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_prov_whole = assert_r9_property(
            fixture_prov,
            &next_prov,
            PatchScope::Whole,
            "R9.6b unknown provider key (Whole)",
        );
        let res_prov_entities = assert_r9_property(
            fixture_prov,
            &next_prov,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.6b unknown provider key (Entities)",
        );

        // 6c: unknown key in model (ModelConfig has deny_unknown_fields)
        let fixture_model = r#"[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
unknown_model_key = 9999
"#;
        let mut next_model: Config = toml::from_str(BASE).expect("BASE is valid Config");
        next_model.models.get_mut("m1").unwrap().max_output_tokens = 2048;
        let res_model_whole = assert_r9_property(
            fixture_model,
            &next_model,
            PatchScope::Whole,
            "R9.6c unknown model key (Whole)",
        );
        let res_model_entities = assert_r9_property(
            fixture_model,
            &next_model,
            PatchScope::Entities {
                providers: &["p1"],
                models: &["m1"],
            },
            "R9.6c unknown model key (Entities)",
        );

        eprintln!(
            "R9.6 outcomes: root_parse=Err(expected), prov_Whole={:?}, prov_Entities={:?}, model_Whole={:?}, model_Entities={:?}",
            res_prov_whole.as_ref().map(|_| "Ok"),
            res_prov_entities.as_ref().map(|_| "Ok"),
            res_model_whole.as_ref().map(|_| "Ok"),
            res_model_entities.as_ref().map(|_| "Ok")
        );
    }

    const R5_FIXTURE: &str = r#"# root 手写注释
[server]
listen = "127.0.0.1:8992"

[providers.p1]
product = "custom"
api_key_env = "P1_KEY"

[providers.p2]
product = "custom"
api_key_env = "P2_KEY"

[providers.p3]
product = "custom"
api_key_env = "P3_KEY"

[models.m1]
context_window = 4096
max_output_tokens = 1024
# 行前注释：绑定顺序敏感
[[models.m1.openai_chat_providers]]
# elem p1
name = "p1"
model = "upstream-a"

[[models.m1.openai_chat_providers]]
# elem p2
name = "p2"
model = "upstream-b"
"#;

    #[test]
    fn r5_1_add_binding_element_readable_and_preserves_existing_bytes() {
        let fixture = R5_FIXTURE;
        let mut next: Config = toml::from_str(fixture).expect("R5_FIXTURE must be valid Config");
        next.models
            .get_mut("m1")
            .unwrap()
            .openai_chat_providers
            .push(crate::config::ProviderBinding {
                name: "p3".to_string(),
                model: "upstream-c".to_string(),
            });

        let rendered = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed for R5.1 add element");
        eprintln!("--- R5.1 rendered ---\n{rendered}");

        // 1. 新元素可读（渲染结果能被解析回预期 Config）
        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse into Config");
        assert_eq!(
            semantic_config_bytes(&parsed).unwrap(),
            semantic_config_bytes(&next).unwrap(),
            "I3: rendered config must be semantically equal to next"
        );
        assert_eq!(parsed.models["m1"].openai_chat_providers.len(), 3);
        assert_eq!(parsed.models["m1"].openai_chat_providers[2].name, "p3");
        assert_eq!(
            parsed.models["m1"].openai_chat_providers[2].model,
            "upstream-c"
        );

        // 2. 既有元素与其注释字节不变
        assert!(
            rendered.contains("# elem p1\nname = \"p1\"\nmodel = \"upstream-a\""),
            "existing p1 element and its comments must remain byte-exact"
        );
        assert!(
            rendered.contains("# elem p2\nname = \"p2\"\nmodel = \"upstream-b\""),
            "existing p2 element and its comments must remain byte-exact"
        );
        assert!(
            rendered.contains("# 行前注释：绑定顺序敏感\n[[models.m1.openai_chat_providers]]"),
            "header comment on array of tables must remain byte-exact"
        );
        assert!(
            rendered.starts_with("# root 手写注释\n"),
            "root comment must remain byte-exact"
        );
        assert!(
            rendered.contains(
                "[[models.m1.openai_chat_providers]]\nname = \"p3\"\nmodel = \"upstream-c\""
            ),
            "new p3 element must be appended to array tail"
        );
    }

    #[test]
    fn r5_2_delete_binding_element_removes_element_and_its_comments_fixture_pinned() {
        let fixture = R5_FIXTURE;
        let mut next: Config = toml::from_str(fixture).expect("R5_FIXTURE must be valid Config");
        next.models
            .get_mut("m1")
            .unwrap()
            .openai_chat_providers
            .retain(|b| b.name != "p2");

        let rendered = render_with_comments(fixture, &next, PatchScope::Whole)
            .expect("render_with_comments must succeed for R5.2 delete element");
        eprintln!("--- R5.2 rendered ---\n{rendered}");

        // 1. 删除生效，渲染结果语义等于 next
        let parsed: Config = toml::from_str(&rendered).expect("rendered must parse into Config");
        assert_eq!(
            semantic_config_bytes(&parsed).unwrap(),
            semantic_config_bytes(&next).unwrap(),
            "I3: rendered config must be semantically equal to next"
        );
        assert_eq!(parsed.models["m1"].openai_chat_providers.len(), 1);
        assert_eq!(parsed.models["m1"].openai_chat_providers[0].name, "p1");

        // 2. 既有 p1 元素及其注释存活
        assert!(
            rendered.contains("# elem p1\nname = \"p1\"\nmodel = \"upstream-a\""),
            "p1 element and its comment must remain intact"
        );

        // 3. 实测记录（2026-09-17）：被删除的 p2 元素及其内部注释随节点一同消失
        assert!(
            !rendered.contains("# elem p2"),
            "R5.2: 记录于 2026-09-17，删除 AoT 元素后其自身注释消失"
        );
        assert!(
            !rendered.contains("upstream-b"),
            "p2 model value must be absent from rendered document"
        );
    }

    #[test]
    fn r5_order_inconsistent_behavior_observed() {
        let fixture = R5_FIXTURE;
        let mut next: Config = toml::from_str(fixture).expect("fixture must be valid Config");
        // Swap p1 and p2 in next
        next.models
            .get_mut("m1")
            .unwrap()
            .openai_chat_providers
            .reverse();

        let result = render_with_comments(fixture, &next, PatchScope::Whole);
        eprintln!("--- R5 order inconsistent result ---\n{result:?}");

        // 实测记录（S4）：遇顺序不一致时，本步不调整顺序也不写 doc_position，
        // I3 语义门检测到渲染结果顺序与 next 不一致，拒绝并返回 Err(Verify)。
        assert!(
            matches!(result, Err(RenderError::Verify { .. })),
            "S4: order inconsistency without reordering must be rejected by I3 Verify"
        );
    }
}
