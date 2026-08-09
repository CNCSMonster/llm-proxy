use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{Config, ModelConfig, Protocol, local_protocol_base_url};
use crate::json_edit;

const QWEN_MANAGED_ENV_KEY: &str = "LLM_PROXY_API_KEY";

#[derive(Debug, Serialize)]
struct PiProvider {
    #[serde(rename = "baseUrl")]
    base_url: String,
    api: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    models: Vec<PiModel>,
}

#[derive(Debug, Serialize)]
struct PiModel {
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(rename = "contextWindow", skip_serializing_if = "is_zero")]
    context_window: i64,
    #[serde(rename = "maxTokens", skip_serializing_if = "is_zero")]
    max_tokens: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input: Vec<String>,
}

pub fn launch_pi(cfg: &Config, pi_home: Option<PathBuf>, dry_run: bool) -> Result<()> {
    cfg.validate()?;
    let models = models_for_protocol(cfg, Protocol::OpenaiChatCompletions);
    if models.is_empty() {
        bail!("配置中没有 pi 可用模型；模型需要暴露 openai-chat-completions");
    }

    let pi_home = pi_home.unwrap_or_else(default_home);
    let path = pi_home.join(".pi").join("agent").join("models.json");
    let mut pi_cfg = json_edit::load_object(&path)?;
    let providers = json_edit::ensure_object(&mut pi_cfg, "providers")?;
    let base_url = local_protocol_base_url(&cfg.server.listen, "/openai/v1");
    let provider = json_edit::to_value(
        PiProvider {
            base_url: base_url.clone(),
            api: "openai-completions".to_string(),
            api_key: "local".to_string(),
            models: models
                .iter()
                .map(|(id, model)| pi_model(id, model))
                .collect(),
        },
        "pi provider",
    )?;
    json_edit::replace_object_entries(
        providers,
        |name, _| name.starts_with("llm-proxy-"),
        [("llm-proxy-openai-chat".to_string(), provider)],
    );

    println!("pi models.json: {}", path.display());
    println!("provider: llm-proxy-openai-chat");
    println!("baseUrl: {base_url}");
    println!("models: {}", models.len());

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &pi_cfg)
}

pub fn launch_qwen_code(
    cfg: &Config,
    model_id: Option<String>,
    qwen_home: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    let models = models_for_protocol(cfg, Protocol::OpenaiChatCompletions);
    if models.is_empty() {
        bail!("配置中没有 Qwen Code 可用模型；模型需要暴露 openai-chat-completions");
    }

    let default_model_id = match model_id {
        Some(id) => {
            if !models.iter().any(|(candidate, _)| *candidate == id) {
                bail!("模型 {id:?} 不是 Qwen Code 可用模型");
            }
            id
        }
        None => models[0].0.to_string(),
    };

    let qwen_home = qwen_home.unwrap_or_else(|| default_home().join(".qwen"));
    let path = qwen_home.join("settings.json");
    let mut settings = json_edit::load_object(&path)?;

    let base_url = local_protocol_base_url(&cfg.server.listen, "/openai/v1");
    let new_items = models
        .iter()
        .map(|(id, model)| {
            json!({
                "id": id,
                "name": id,
                "description": format!("{id} via llm-proxy"),
                "baseUrl": base_url,
                "envKey": QWEN_MANAGED_ENV_KEY,
                "generationConfig": {
                    "contextWindowSize": model.context_window,
                    "maxOutputTokens": model.max_output_tokens,
                },
            })
        })
        .collect();
    let providers = json_edit::ensure_object(&mut settings, "modelProviders")?;
    json_edit::replace_array_items(
        providers,
        "openai",
        |item| is_qwen_managed_model(item, &base_url),
        new_items,
    );

    json_edit::set_value(
        &mut settings,
        &["env", QWEN_MANAGED_ENV_KEY],
        json!("local"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["security", "auth", "selectedType"],
        json!("openai"),
    )?;
    json_edit::set_value(&mut settings, &["model", "name"], json!(default_model_id))?;
    json_edit::set_value(&mut settings, &["model", "baseUrl"], json!(base_url))?;

    println!("Qwen Code settings.json: {}", path.display());
    println!("baseUrl[openai]: {base_url}");
    println!("modelProviders: {} llm-proxy models", models.len());

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)
}

pub fn launch_claude_code(
    cfg: &Config,
    model_id: Option<String>,
    claude_home: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    let models = models_for_protocol(cfg, Protocol::Anthropic);
    if models.is_empty() {
        bail!("配置中没有 Claude Code 可用模型；需要模型暴露 anthropic provider binding");
    }

    let (selected_model_id, selected_model) = match model_id {
        Some(id) => models
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("模型 {id:?} 不是 Claude Code 可用模型"))?,
        None => models[0],
    };

    let claude_home = claude_home.unwrap_or_else(|| default_home().join(".claude"));
    let path = claude_home.join("settings.json");
    let mut settings = json_edit::load_object(&path)?;
    let base_url = local_protocol_base_url(&cfg.server.listen, "/anthropic/v1")
        .trim_end_matches("/v1")
        .to_string();
    let suffix = if selected_model.context_window >= 1_000_000 {
        "[1m]"
    } else {
        ""
    };

    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_BASE_URL"],
        json!(base_url),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_AUTH_TOKEN"],
        json!("local"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_MODEL"],
        json!(format!("sonnet{suffix}")),
    )?;
    json_edit::set_value(
        &mut settings,
        &["env", "ANTHROPIC_DEFAULT_SONNET_MODEL"],
        json!(format!("{selected_model_id}{suffix}")),
    )?;
    if selected_model.max_output_tokens > 0 {
        json_edit::set_value(
            &mut settings,
            &["env", "CLAUDE_CODE_MAX_OUTPUT_TOKENS"],
            json!(selected_model.max_output_tokens.to_string()),
        )?;
    }

    println!("Claude Code settings.json: {}", path.display());
    println!("ANTHROPIC_BASE_URL: {base_url}");
    println!("ANTHROPIC_DEFAULT_SONNET_MODEL: {selected_model_id}{suffix}");

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)
}

pub fn launch_claude_desktop(
    cfg: &Config,
    claude_desktop_home: Option<PathBuf>,
    profile: String,
    dry_run: bool,
) -> Result<()> {
    cfg.validate()?;
    let models = models_for_protocol(cfg, Protocol::Anthropic);
    if models.is_empty() {
        bail!("配置中没有 Claude Desktop 可用模型；需要模型暴露 anthropic provider binding");
    }

    let home = claude_desktop_home.unwrap_or_else(default_claude_desktop_home);
    let path = home.join("claude_desktop_config.json");
    let mut settings = json_edit::load_object(&path)?;
    let base_url = local_protocol_base_url(&cfg.server.listen, "/anthropic/v1")
        .trim_end_matches("/v1")
        .to_string();
    let model_ids: Vec<_> = models.iter().map(|(id, _)| (*id).to_string()).collect();
    let default_model = model_ids.first().cloned().unwrap_or_default();

    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "type"],
        json!("anthropic"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "baseUrl"],
        json!(base_url),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "apiKey"],
        json!("local"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "providers", "llm-proxy", "models"],
        json!(model_ids),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "profiles", &profile, "provider"],
        json!("llm-proxy"),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "profiles", &profile, "defaultModel"],
        json!(default_model),
    )?;
    json_edit::set_value(
        &mut settings,
        &["llmProxy", "activeProfile"],
        json!(profile),
    )?;

    println!("Claude Desktop config: {}", path.display());
    println!(
        "profile: {}",
        settings["llmProxy"]["activeProfile"]
            .as_str()
            .unwrap_or("llm-proxy")
    );
    println!(
        "provider baseUrl: {}",
        settings["llmProxy"]["providers"]["llm-proxy"]["baseUrl"]
            .as_str()
            .unwrap_or("")
    );
    println!("models: {}", models.len());

    if dry_run {
        println!("[dry-run] would write {}", path.display());
        return Ok(());
    }

    json_edit::write_object(&path, &settings)
}

fn default_claude_desktop_home() -> PathBuf {
    if cfg!(target_os = "macos") {
        default_home()
            .join("Library")
            .join("Application Support")
            .join("Claude")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(default_home)
            .join("Claude")
    } else {
        default_home().join(".config").join("Claude")
    }
}

fn models_for_protocol(cfg: &Config, protocol: Protocol) -> Vec<(&str, &ModelConfig)> {
    let mut models: Vec<_> = cfg
        .models
        .iter()
        .filter(|(_, model)| model.exposes_protocol(protocol))
        .map(|(id, model)| (id.as_str(), model))
        .collect();
    models.sort_by(|a, b| a.0.cmp(b.0));
    models
}

fn pi_model(id: &str, model: &ModelConfig) -> PiModel {
    PiModel {
        id: id.to_string(),
        name: id.to_string(),
        context_window: model.context_window,
        max_tokens: model.max_output_tokens,
        input: if model
            .features
            .iter()
            .any(|feature| feature == "image_input")
        {
            vec!["text".to_string(), "image".to_string()]
        } else {
            Vec::new()
        },
    }
}

fn is_qwen_managed_model(item: &Value, base_url: &str) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };
    obj.get("envKey").and_then(Value::as_str) == Some(QWEN_MANAGED_ENV_KEY)
        || obj.get("baseUrl").and_then(Value::as_str) == Some(base_url)
}

fn default_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use std::fs;

    #[test]
    fn launch_pi_writes_openai_chat_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join(".pi/agent/models.json")).expect("read pi");
        assert!(text.contains("\"llm-proxy-openai-chat\""));
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/openai/v1\""));
        assert!(text.contains("\"id\": \"deepseek-v4-pro-lp\""));
    }

    #[test]
    fn launch_pi_preserves_existing_incomplete_custom_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/agent/models.json");
        fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "providers": {
    "custom": {
      "baseUrl": "https://example.test/v1",
      "api": "openai-completions",
      "models": [
        { "id": "custom-model" }
      ]
    },
    "llm-proxy-old": {
      "baseUrl": "http://old/openai/v1",
      "api": "openai-completions",
      "models": []
    }
  }
}
"#,
        )
        .expect("write existing pi config");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(path).expect("read pi");
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains("\"custom\""));
        assert!(text.contains("\"custom-model\""));
        assert!(!text.contains("\"llm-proxy-old\""));
        assert!(text.contains("\"llm-proxy-openai-chat\""));
    }

    #[test]
    fn launch_qwen_code_writes_openai_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, None, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let text = fs::read_to_string(temp.path().join("settings.json")).expect("read qwen");
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/openai/v1\""));
        assert!(text.contains("\"selectedType\": \"openai\""));
        assert!(text.contains("\"name\": \"deepseek-v4-pro-lp\""));
        assert!(text.contains("\"contextWindowSize\": 1000000"));
        assert!(!text.contains("\"contextWindow\": 1000000"));
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/openai/v1\""));
    }

    #[test]
    fn launch_qwen_code_preserves_unmanaged_settings_and_models() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "modelProviders": {
    "openai": [
      {
        "id": "custom-openai",
        "baseUrl": "https://example.test/v1",
        "envKey": "CUSTOM_KEY"
      },
      {
        "id": "old-managed",
        "baseUrl": "http://127.0.0.1:8989/openai/v1",
        "envKey": "LLM_PROXY_API_KEY"
      }
    ]
  }
}
"#,
        )
        .expect("write existing qwen settings");
        let cfg = config::default_deepseek_config();

        launch_qwen_code(&cfg, None, Some(temp.path().to_path_buf()), false).expect("launch qwen");

        let text = fs::read_to_string(path).expect("read qwen");
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"custom-openai\""));
        assert!(!text.contains("\"old-managed\""));
        assert!(text.contains("\"deepseek-v4-pro-lp\""));
    }

    #[test]
    fn launch_pi_uses_server_listen_base_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_pi(&cfg, Some(temp.path().to_path_buf()), false).expect("launch pi");

        let text = fs::read_to_string(temp.path().join(".pi/agent/models.json")).expect("read pi");
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/openai/v1\""));
    }

    #[test]
    fn launch_claude_desktop_writes_profile_and_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_claude_desktop(
            &cfg,
            Some(temp.path().to_path_buf()),
            "coding".to_string(),
            false,
        )
        .expect("launch claude desktop");

        let text = fs::read_to_string(temp.path().join("claude_desktop_config.json"))
            .expect("read desktop config");
        assert!(text.contains("\"activeProfile\": \"coding\""));
        assert!(text.contains("\"baseUrl\": \"http://127.0.0.1:8989/anthropic\""));
        assert!(text.contains("deepseek-v4-pro-lp"));
    }

    #[test]
    fn launch_claude_desktop_preserves_unmanaged_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("claude_desktop_config.json");
        fs::write(
            &path,
            r#"{"theme":"dark","llmProxy":{"profiles":{"custom":{"provider":"other"}}}}"#,
        )
        .expect("write existing desktop config");
        let cfg = config::default_deepseek_config();

        launch_claude_desktop(
            &cfg,
            Some(temp.path().to_path_buf()),
            "llm-proxy".to_string(),
            false,
        )
        .expect("launch claude desktop");

        let text = fs::read_to_string(path).expect("read desktop config");
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"custom\""));
        assert!(text.contains("http://127.0.0.1:8989/anthropic"));
    }

    #[test]
    fn launch_claude_code_writes_anthropic_settings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = config::default_deepseek_config();

        launch_claude_code(
            &cfg,
            Some("deepseek-v4-pro-lp".to_string()),
            Some(temp.path().to_path_buf()),
            false,
        )
        .expect("launch claude code");

        let text = fs::read_to_string(temp.path().join("settings.json")).expect("read settings");
        assert!(text.contains("\"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:8989/anthropic\""));
        assert!(text.contains("\"ANTHROPIC_AUTH_TOKEN\": \"local\""));
        assert!(text.contains("\"ANTHROPIC_MODEL\": \"sonnet[1m]\""));
        assert!(text.contains("\"ANTHROPIC_DEFAULT_SONNET_MODEL\": \"deepseek-v4-pro-lp[1m]\""));
    }
}
