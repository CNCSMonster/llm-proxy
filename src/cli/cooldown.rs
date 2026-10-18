use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Result, bail};

use crate::cli::types::CooldownCommand;
use crate::config::Config;
use crate::cooldown;
use crate::pattern::{self, PatternMatcher};

/// Entry point for `llm-proxy cooldown <subcommand>`.
pub async fn run_cooldown_command(config_path: &Path, command: CooldownCommand) -> Result<()> {
    let cooldown_config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let cooldown_dirs = crate::service::StateDirs::resolve(cooldown_config_dir);
    match command {
        CooldownCommand::List => {
            let cfg = Config::load(config_path).ok();
            cooldown::print_list(
                &cooldown_dirs.state_dir.join("cooldowns.json"),
                cfg.as_ref(),
            )?;
            Ok(())
        }
        CooldownCommand::Set {
            provider,
            model,
            duration,
            protocol,
            kind,
            reason,
            dry_run,
            yes,
        } => {
            run_cooldown_set(
                config_path,
                provider,
                model,
                duration,
                protocol,
                kind,
                reason,
                dry_run,
                yes,
            )
            .await
        }
        CooldownCommand::Clear {
            provider,
            model,
            dry_run,
            yes,
        } => run_cooldown_clear(config_path, provider, model, dry_run, yes).await,
    }
}

#[allow(dead_code)]
pub(crate) fn is_glob_pattern(pattern: &str) -> bool {
    pattern::is_glob_pattern(pattern)
}

pub(crate) fn resolve_models(
    cfg: &Config,
    provider: &str,
    model: Option<&str>,
) -> Result<(Vec<String>, bool)> {
    if !cfg.providers.contains_key(provider) {
        bail!("unknown provider {provider:?}");
    }

    match model {
        None => {
            let models = cfg.models_for_provider(provider);
            if models.is_empty() {
                bail!("no models found for provider {provider:?}");
            }
            Ok((models, true))
        }
        Some(pat) => {
            let matcher = PatternMatcher::new(pat).map_err(|e| anyhow::anyhow!("{e}"))?;
            if matcher.is_glob() {
                let all_models = cfg.models_for_provider(provider);
                let matched: Vec<String> = all_models
                    .into_iter()
                    .filter(|m| matcher.is_match(m))
                    .collect();
                if matched.is_empty() {
                    bail!("no models match pattern {pat:?} for provider {provider:?}");
                }
                Ok((matched, true))
            } else {
                Ok((vec![pat.to_string()], false))
            }
        }
    }
}

pub(crate) fn print_dry_run(action: &str, models: &[String]) {
    let noun = if models.len() == 1 { "model" } else { "models" };
    println!("Would {action} cooldown for {} {noun}:", models.len());
    for m in models {
        println!("  - {m}");
    }
}

pub(crate) fn confirm_batch<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    action: &str,
    models: &[String],
    is_terminal: bool,
    yes: bool,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_terminal {
        bail!(
            "batch cooldown operation in non-interactive environment requires --yes or --dry-run"
        );
    }
    let noun = if models.len() == 1 { "model" } else { "models" };
    writeln!(
        writer,
        "Will {action} cooldown for {} {noun}:",
        models.len()
    )?;
    for m in models {
        writeln!(writer, "  - {m}")?;
    }
    write!(writer, "Proceed? [y/N] ")?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
        Ok(true)
    } else {
        writeln!(writer, "Cancelled.")?;
        Ok(false)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cooldown_set(
    config_path: &Path,
    provider: String,
    model: Option<String>,
    duration: String,
    protocol: Option<String>,
    kind: Option<String>,
    reason: Option<String>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let cooldown_dirs = crate::service::StateDirs::resolve(
        config_path.parent().unwrap_or(std::path::Path::new(".")),
    );
    // 1. Validate duration & protocol early
    cooldown::parse_manual_duration(&duration)?;
    cooldown::protocols_for_cli(protocol.as_deref())?;

    // 2. Load config and resolve models
    let cfg = Config::load(config_path)?;
    let (models, is_batch) = resolve_models(&cfg, &provider, model.as_deref())?;

    // 3. Dry-run
    if dry_run {
        print_dry_run("set", &models);
        return Ok(());
    }

    // 4. Batch confirmation / non-TTY safety guard
    if is_batch {
        let is_terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
        let confirmed = confirm_batch(
            &mut io::stdin().lock(),
            &mut io::stdout(),
            "set",
            &models,
            is_terminal,
            yes,
        )?;
        if !confirmed {
            return Ok(());
        }
    }

    // 5. Execute set under write lock or via delegate
    crate::ownership::with_cli_write_lock_or_delegate(
        config_path,
        "llm-proxy cooldown set",
        || {
            let provider = provider.clone();
            let models = models.clone();
            let duration = duration.clone();
            let protocol = protocol.clone();
            let kind = kind.clone();
            let reason = reason.clone();
            let cooldown_state_path = cooldown_dirs.state_dir.join("cooldowns.json");
            async move {
                let state_path = cooldown_state_path;
                for m in &models {
                    let result = cooldown::set_from_cli(
                        &state_path,
                        &provider,
                        m,
                        &duration,
                        protocol.as_deref(),
                        kind.as_deref(),
                        reason.as_deref(),
                    )?;
                    println!("{}", cooldown::format_set_message(&result));
                }
                Ok(())
            }
        },
        |server| {
            let provider = provider.clone();
            let models = models.clone();
            let duration = duration.clone();
            let protocol = protocol.clone();
            let kind = kind.clone();
            let reason = reason.clone();
            Box::pin(async move {
                for m in &models {
                    let result = server
                        .cooldown_set(
                            &provider,
                            m,
                            &duration,
                            protocol.as_deref(),
                            kind.as_deref(),
                            reason.as_deref(),
                        )
                        .await?;
                    let data = result.get("data").cloned().unwrap_or_default();
                    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let kind = data
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or(cooldown::DEFAULT_MANUAL_KIND);
                    let duration_seconds = data
                        .get("duration_seconds")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let protocols = data
                        .get("protocols")
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    println!(
                        "Set {count} cooldown(s) for model={m} provider={provider} duration={duration_seconds}s kind={kind} protocol=[{protocols}]"
                    );
                }
                Ok(())
            })
        },
    )
    .await?;

    Ok(())
}

async fn run_cooldown_clear(
    config_path: &Path,
    provider: String,
    model: Option<String>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let cooldown_dirs = crate::service::StateDirs::resolve(
        config_path.parent().unwrap_or(std::path::Path::new(".")),
    );
    // 1. Load config and resolve models
    let cfg = Config::load(config_path)?;
    let (models, is_batch) = resolve_models(&cfg, &provider, model.as_deref())?;

    // 2. Dry-run
    if dry_run {
        print_dry_run("clear", &models);
        return Ok(());
    }

    // 3. Batch confirmation / non-TTY safety guard
    if is_batch {
        let is_terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
        let confirmed = confirm_batch(
            &mut io::stdin().lock(),
            &mut io::stdout(),
            "clear",
            &models,
            is_terminal,
            yes,
        )?;
        if !confirmed {
            return Ok(());
        }
    }

    // 4. Execute clear under write lock or via delegate
    crate::ownership::with_cli_write_lock_or_delegate(
        config_path,
        "llm-proxy cooldown clear",
        || {
            let provider = provider.clone();
            let models = models.clone();
            let clear_cooldown_path = cooldown_dirs.state_dir.join("cooldowns.json");
            async move {
                if !is_batch {
                    let m = &models[0];
                    let removed =
                        cooldown::clear(&clear_cooldown_path, Some(m.as_str()), &provider)?;
                    if removed == 0 {
                        println!("No cooldown found for provider={provider}");
                    } else {
                        println!("Cleared cooldown for model={m} provider={provider}");
                    }
                } else {
                    for m in &models {
                        let removed =
                            cooldown::clear(&clear_cooldown_path, Some(m.as_str()), &provider)?;
                        if removed > 0 {
                            println!(
                                "Cleared {removed} cooldown(s) for model={m} provider={provider}"
                            );
                        } else {
                            println!("No cooldown found for model={m} provider={provider}");
                        }
                    }
                }
                Ok(())
            }
        },
        |server| {
            let provider = provider.clone();
            let models = models.clone();
            Box::pin(async move {
                if !is_batch {
                    let m = &models[0];
                    let result = server.cooldown_clear(Some(m.as_str()), &provider).await?;
                    let removed = result
                        .get("data")
                        .and_then(|d| d.get("removed"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    if removed == 0 {
                        println!("No cooldown found for provider={provider}");
                    } else {
                        println!("Cleared cooldown for model={m} provider={provider}");
                    }
                } else {
                    for m in &models {
                        let result = server.cooldown_clear(Some(m.as_str()), &provider).await?;
                        let removed = result
                            .get("data")
                            .and_then(|d| d.get("removed"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        if removed > 0 {
                            println!(
                                "Cleared {removed} cooldown(s) for model={m} provider={provider}"
                            );
                        } else {
                            println!("No cooldown found for model={m} provider={provider}");
                        }
                    }
                }
                Ok(())
            })
        },
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.ubt-bailian]
api_key_env = "KEY"
[providers.ubt-bailian.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.openai]
api_key_env = "KEY"
[providers.openai.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[providers.empty-prov]
api_key_env = "KEY"
[providers.empty-prov.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.qwen-flash]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "ubt-bailian", model = "qwen-flash" }]

[models.qwen-turbo]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "ubt-bailian", model = "qwen-turbo" }]

[models.qwen3-8-flash]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "ubt-bailian", model = "qwen3.8-flash" }]

[models.gpt-4]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "openai", model = "gpt-4" }]
"#;
        toml::from_str(toml_str).expect("parse test config")
    }

    #[test]
    fn test_is_glob_pattern() {
        assert!(is_glob_pattern("qwen-*"));
        assert!(is_glob_pattern("qwen3.?"));
        assert!(is_glob_pattern("qwen[1-3]"));
        assert!(!is_glob_pattern("qwen-flash"));
        assert!(!is_glob_pattern("gpt-4"));
    }

    #[test]
    fn test_resolve_models_unknown_provider() {
        let cfg = test_config();
        let err = resolve_models(&cfg, "nonexistent", None).unwrap_err();
        assert!(err.to_string().contains("unknown provider \"nonexistent\""));
    }

    #[test]
    fn test_resolve_models_empty_provider() {
        let cfg = test_config();
        let err = resolve_models(&cfg, "empty-prov", None).unwrap_err();
        assert!(
            err.to_string()
                .contains("no models found for provider \"empty-prov\"")
        );
    }

    #[test]
    fn test_resolve_models_omitted_returns_all_models_for_provider() {
        let cfg = test_config();
        let (models, is_batch) = resolve_models(&cfg, "ubt-bailian", None).unwrap();
        assert!(is_batch);
        assert_eq!(models, vec!["qwen-flash", "qwen-turbo", "qwen3-8-flash"]);
    }

    #[test]
    fn test_resolve_models_glob_matches_subset() {
        let cfg = test_config();
        let (models, is_batch) = resolve_models(&cfg, "ubt-bailian", Some("qwen-*")).unwrap();
        assert!(is_batch);
        assert_eq!(models, vec!["qwen-flash", "qwen-turbo"]);
    }

    #[test]
    fn test_resolve_models_glob_single_char_wildcard() {
        let cfg = test_config();
        // `qwen?-8-flash` matches `qwen3-8-flash`
        let (models, is_batch) =
            resolve_models(&cfg, "ubt-bailian", Some("qwen?-8-flash")).unwrap();
        assert!(is_batch);
        assert_eq!(models, vec!["qwen3-8-flash"]);
    }

    #[test]
    fn test_resolve_models_glob_empty_match_fails() {
        let cfg = test_config();
        let err = resolve_models(&cfg, "ubt-bailian", Some("nonexistent-*")).unwrap_err();
        assert!(
            err.to_string()
                .contains("no models match pattern \"nonexistent-*\"")
        );
    }

    #[test]
    fn test_resolve_models_exact_model_is_not_batch() {
        let cfg = test_config();
        let (models, is_batch) = resolve_models(&cfg, "ubt-bailian", Some("qwen-flash")).unwrap();
        assert!(!is_batch);
        assert_eq!(models, vec!["qwen-flash"]);
    }

    #[test]
    fn test_confirm_batch_with_yes_skips_prompt() {
        let mut input = io::Cursor::new(b"");
        let mut output = Vec::new();
        let models = vec!["m1".to_string(), "m2".to_string()];
        let ok = confirm_batch(&mut input, &mut output, "set", &models, false, true).unwrap();
        assert!(ok);
        assert!(output.is_empty());
    }

    #[test]
    fn test_confirm_batch_non_interactive_without_yes_errors() {
        let mut input = io::Cursor::new(b"");
        let mut output = Vec::new();
        let models = vec!["m1".to_string(), "m2".to_string()];
        let err = confirm_batch(&mut input, &mut output, "set", &models, false, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("non-interactive environment requires --yes or --dry-run")
        );
    }

    #[test]
    fn test_confirm_batch_interactive_accepts_y() {
        let mut input = io::Cursor::new(b"y\n");
        let mut output = Vec::new();
        let models = vec!["m1".to_string(), "m2".to_string()];
        let ok = confirm_batch(&mut input, &mut output, "set", &models, true, false).unwrap();
        assert!(ok);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Will set cooldown for 2 models:"));
        assert!(out_str.contains("  - m1"));
        assert!(out_str.contains("  - m2"));
        assert!(out_str.contains("Proceed? [y/N]"));
    }

    #[test]
    fn test_confirm_batch_interactive_accepts_yes() {
        let mut input = io::Cursor::new(b"YES\n");
        let mut output = Vec::new();
        let models = vec!["m1".to_string()];
        let ok = confirm_batch(&mut input, &mut output, "clear", &models, true, false).unwrap();
        assert!(ok);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Will clear cooldown for 1 model:"));
    }

    #[test]
    fn test_confirm_batch_interactive_rejects_n() {
        let mut input = io::Cursor::new(b"n\n");
        let mut output = Vec::new();
        let models = vec!["m1".to_string()];
        let ok = confirm_batch(&mut input, &mut output, "set", &models, true, false).unwrap();
        assert!(!ok);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Cancelled."));
    }

    #[tokio::test]
    async fn test_run_cooldown_set_dry_run_does_not_modify_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let toml_str = r#"
[server]
listen = "127.0.0.1:8989"

[providers.ubt-bailian]
api_key_env = "KEY"
[providers.ubt-bailian.openai_chat]
url = "https://api.example.com/v1/chat/completions"

[models.qwen-flash]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "ubt-bailian", model = "qwen-flash" }]

[models.qwen-turbo]
context_window = 1000
max_output_tokens = 100
openai_chat_providers = [{ name = "ubt-bailian", model = "qwen-turbo" }]
"#;
        std::fs::write(&config_path, toml_str).expect("write config");

        // Dry-run set for all models
        let res = run_cooldown_set(
            &config_path,
            "ubt-bailian".to_string(),
            None,
            "720h".to_string(),
            None,
            None,
            None,
            true, // dry_run
            false,
        )
        .await;
        assert!(res.is_ok());

        // Dry-run set with glob
        let res_glob = run_cooldown_set(
            &config_path,
            "ubt-bailian".to_string(),
            Some("qwen-*".to_string()),
            "720h".to_string(),
            None,
            None,
            None,
            true, // dry_run
            false,
        )
        .await;
        assert!(res_glob.is_ok());

        // Dry-run clear
        let res_clear = run_cooldown_clear(
            &config_path,
            "ubt-bailian".to_string(),
            None,
            true, // dry_run
            false,
        )
        .await;
        assert!(res_clear.is_ok());
    }

    #[tokio::test]
    async fn test_run_cooldown_set_early_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write config");

        // Invalid duration fails immediately
        let res = run_cooldown_set(
            &config_path,
            "ubt-bailian".to_string(),
            None,
            "invalid_duration".to_string(),
            None,
            None,
            None,
            false,
            false,
        )
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid duration"));
    }
}
