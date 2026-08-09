use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::{
    self, AppModel, FallbackConfigState, FallbackFocus, FallbackOption, Screen, VerifyingState,
};
use crate::tui::fuzzy::fuzzy_match;
use crate::tui::update::models::get_model_templates;
use crate::tui::update::utils::{handle_filter_input, scan_env_vars};

const MAX_RECOMMENDED: usize = 3;

/// Seed for env var recommendation with associated weight.
struct RecommendSeed {
    pattern: String,
    weight: i32,
}

/// Compute similarity score between an env var name and a seed.
/// Returns a weighted score: seed_weight * match_type / 100.
fn env_similarity_score(env_name: &str, seed: &RecommendSeed) -> i32 {
    let env_upper = env_name.to_uppercase();
    let seed_upper = seed.pattern.to_uppercase();

    let match_score = if env_upper == seed_upper {
        100 // exact match
    } else if env_upper.starts_with(&seed_upper) || seed_upper.starts_with(&env_upper) {
        90 // prefix match
    } else if env_upper.contains(&seed_upper) || seed_upper.contains(&env_upper) {
        80 // contains
    } else if fuzzy_match(&seed.pattern, env_name) {
        60 // fuzzy
    } else {
        return 0;
    };

    seed.weight * match_score / 100
}

/// Compute recommendation scores for all env items and mark top N as recommended.
/// Returns items sorted: recommended first (by score desc), then alphabetical.
pub(crate) fn apply_recommendations(items: &mut Vec<model::EnvItem>, product_id: &str) {
    // Build recommendation seeds from catalog
    let seeds = build_recommendation_seeds(product_id);
    if seeds.is_empty() {
        return;
    }

    // Compute scores for non-skip items
    let mut scored: Vec<(usize, i32)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.is_skip)
        .map(|(idx, item)| {
            let max_score = seeds
                .iter()
                .map(|seed| env_similarity_score(&item.name, seed))
                .max()
                .unwrap_or(0);
            (idx, max_score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();

    // Sort by score descending
    scored.sort_by_key(|a| std::cmp::Reverse(a.1));

    // Mark top N as recommended
    let recommended_indices: Vec<usize> = scored
        .iter()
        .take(MAX_RECOMMENDED)
        .map(|(idx, _)| *idx)
        .collect();

    for idx in &recommended_indices {
        items[*idx].recommended = true;
    }

    // Stable sort: recommended items first (preserving score order), then alphabetical
    // We need to partition and reassemble
    let skip_item = items.iter().find(|i| i.is_skip).cloned();
    let mut recommended: Vec<model::EnvItem> = Vec::new();
    let mut normal: Vec<model::EnvItem> = Vec::new();

    for item in items.drain(..) {
        if item.is_skip {
            continue;
        }
        if item.recommended {
            recommended.push(item);
        } else {
            normal.push(item);
        }
    }

    // Recommended items are already in score order from the indices
    // Normal items stay alphabetical (original order)
    items.clear();
    items.extend(recommended);
    items.extend(normal);
    if let Some(skip) = skip_item {
        items.push(skip);
    }
}

/// Build recommendation seeds from catalog for a given product.
fn build_recommendation_seeds(product_id: &str) -> Vec<RecommendSeed> {
    let entries = crate::catalog::built_in_providers();
    let entry = entries.iter().find(|e| e.id == product_id);

    let mut seeds = Vec::new();

    if let Some(entry) = entry {
        // Seed 1: api_key_env (weight 100)
        if let Some(ref api_key_env) = entry.provider.api_key_env {
            seeds.push(RecommendSeed {
                pattern: api_key_env.clone(),
                weight: 100,
            });
        }

        // Seed 2: product ID (weight 80) — used as provider name in config
        seeds.push(RecommendSeed {
            pattern: entry.id.to_string(),
            weight: 80,
        });
    }

    seeds
}

pub(crate) fn make_env_state(app: &AppModel) -> model::EnvVarSelectionState {
    let mut items = scan_env_vars();

    // Apply recommendations based on chosen product
    if let Some(ref product) = app.chosen_product {
        apply_recommendations(&mut items, &product.id);
    }

    let mut state = model::EnvVarSelectionState {
        items,
        cursor: 0,
        filter: String::new(),
        filter_active: false,
        error: None,
        product_name: app
            .chosen_product
            .as_ref()
            .map(|p| p.display_name.clone())
            .unwrap_or_default(),
        env_in_use_warning: None,
    };
    // Compute initial warning for cursor position
    compute_env_warning(&mut state, &app.config_path);
    state
}

/// Compute the env-in-use warning for the currently highlighted env var.
pub(crate) fn compute_env_warning(
    state: &mut model::EnvVarSelectionState,
    config_path: &std::path::Path,
) {
    state.env_in_use_warning = None;
    if let Some(item) = state.items.get(state.cursor) {
        if item.is_skip {
            return;
        }
        let providers = crate::config::Config::load(config_path)
            .map(|cfg| cfg.providers)
            .unwrap_or_default();
        if let Some(provider_name) = crate::tui::model::find_env_user(&providers, &item.name) {
            state.env_in_use_warning = Some(format!("⚠ 该 key 已被 {} 使用", provider_name));
        }
    }
}

pub(crate) fn handle_env_keys(app: &mut AppModel, key: KeyEvent) {
    // If filter is active, only handle filter keys and Enter
    {
        let filter_active = if let Screen::EnvVarSelection(ref s) = app.screen {
            s.filter_active
        } else {
            false
        };
        if filter_active {
            match key.code {
                KeyCode::Esc => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.deactivate_filter();
                    }
                    return;
                }
                KeyCode::Enter => {
                    confirm_env_selection(app);
                    return;
                }
                KeyCode::Up => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                KeyCode::Down => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                KeyCode::Char('k') if key.modifiers.is_empty() => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                KeyCode::Char('j') if key.modifiers.is_empty() => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_up();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        s.move_down();
                    }
                    update_env_warning_from_screen(app);
                    return;
                }
                _ => {
                    if let Screen::EnvVarSelection(ref mut s) = app.screen {
                        handle_filter_input(&mut s.filter, key);
                    }
                    return;
                }
            }
        }
    }

    // Normal mode
    match key.code {
        KeyCode::Enter => {
            confirm_env_selection(app);
        }
        KeyCode::Char('s') => {
            app.chosen_env_var = None;
            advance_from_env(app);
        }
        KeyCode::Char('/') => {
            if let Screen::EnvVarSelection(ref mut s) = app.screen {
                s.activate_filter();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Screen::EnvVarSelection(ref mut s) = app.screen {
                s.move_up();
            }
            update_env_warning_from_screen(app);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Screen::EnvVarSelection(ref mut s) = app.screen {
                s.move_down();
            }
            update_env_warning_from_screen(app);
        }
        _ => {}
    }
}

fn confirm_env_selection(app: &mut AppModel) {
    let cursor = if let Screen::EnvVarSelection(ref s) = app.screen {
        s.cursor
    } else {
        return;
    };
    let env_var = if let Screen::EnvVarSelection(ref s) = app.screen {
        let item = &s.items[cursor];
        if item.is_skip {
            None
        } else {
            Some(item.name.clone())
        }
    } else {
        None
    };
    app.chosen_env_var = env_var;
    advance_from_env(app);
}

/// Helper: recompute env_in_use_warning from the current screen state.
fn update_env_warning_from_screen(app: &mut AppModel) {
    let config_path = app.config_path.clone();
    if let Screen::EnvVarSelection(ref mut s) = app.screen {
        compute_env_warning(s, &config_path);
    }
}

fn advance_from_env(app: &mut AppModel) {
    let product_id = app.chosen_product.as_ref().map(|p| p.id.clone());
    let product_name = app
        .chosen_product
        .as_ref()
        .map(|p| p.display_name.clone())
        .unwrap_or_default();

    // Repeat connect: route to FallbackConfig (default) after env selection
    if app.is_repeat_connect {
        let fallback_state = build_fallback_state(app);
        app.screen = Screen::FallbackConfig(fallback_state);
        return;
    }

    // First-time connect: route to ModelSelection (or Verifying if no templates)
    let models = get_model_templates(product_id.as_deref());

    if models.is_empty() {
        app.screen = Screen::Verifying(VerifyingState {
            product_name,
            env_var: app.chosen_env_var.clone(),
            models: vec![],
            force_local: false,
        });
    } else {
        let configured = if let Some(ref product) = app.chosen_product {
            model::build_configured_set(&app.config_path, &product.id, &models)
        } else {
            HashSet::new()
        };
        app.screen = Screen::ModelSelection(model::ModelSelectionState {
            items: models,
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            selected: HashSet::new(),
            configured,
            error: None,
            product_name,
        });
    }
}

/// Build the FallbackConfigState for the current product.
pub(crate) fn build_fallback_state(app: &AppModel) -> FallbackConfigState {
    let product_id = app
        .chosen_product
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_default();
    let provider_name = app.oauth_provider_name.clone().unwrap_or_default();

    // Find other providers of the same product (exclude the one just created)
    let providers = crate::config::Config::load(&app.config_path)
        .map(|cfg| cfg.providers)
        .unwrap_or_default();
    let target_providers: Vec<String> =
        crate::tui::model::find_providers_by_product(&providers, &product_id)
            .into_iter()
            .filter(|name| name != &provider_name)
            .collect();

    // Build (model, endpoint) options from actual config models (not just templates).
    // This ensures custom models and user-added models are included.
    let config = crate::config::Config::load(&app.config_path).ok();
    let mut options = Vec::new();
    if let Some(ref cfg) = config {
        for (model_id, model_config) in &cfg.models {
            for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
                let bindings = model_config.provider_bindings(protocol);
                // Check if the current provider has a binding in this chain
                let has_provider = bindings.iter().any(|b| b.name == provider_name);
                if has_provider {
                    let endpoint_label = match protocol {
                        crate::config::Protocol::OpenaiChatCompletions => "chat",
                        crate::config::Protocol::OpenaiResponses => "responses",
                        crate::config::Protocol::Anthropic => "anthropic",
                        _ => continue,
                    };
                    // Use model_id as display name (no catalog template available here)
                    options.push(FallbackOption {
                        model_id: model_id.clone(),
                        model_display_name: model_id.clone(),
                        endpoint: endpoint_label.to_string(),
                        selected: false,
                    });
                }
            }
        }
    }

    FallbackConfigState {
        target_providers,
        target_cursor: 0,
        options,
        option_cursor: 0,
        focus: FallbackFocus::TargetProvider,
        error: None,
        skipped: false,
        source: crate::tui::model::FallbackSource::ConnectFlow,
    }
}

// catalog_available_endpoints removed: build_fallback_state now uses actual config models
// instead of catalog templates, so this function is no longer needed.

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(name: &str) -> model::EnvItem {
        model::EnvItem {
            name: name.to_string(),
            is_skip: false,
            recommended: false,
        }
    }

    fn make_skip() -> model::EnvItem {
        model::EnvItem {
            name: String::new(),
            is_skip: true,
            recommended: false,
        }
    }

    #[test]
    fn test_env_similarity_exact_match() {
        let seed = RecommendSeed {
            pattern: "DEEPSEEK_API_KEY".to_string(),
            weight: 100,
        };
        assert_eq!(env_similarity_score("DEEPSEEK_API_KEY", &seed), 100);
    }

    #[test]
    fn test_env_similarity_prefix_match() {
        let seed = RecommendSeed {
            pattern: "DEEPSEEK".to_string(),
            weight: 100,
        };
        // "DEEPSEEK_API_KEY" starts with "DEEPSEEK"
        assert_eq!(env_similarity_score("DEEPSEEK_API_KEY", &seed), 90);
    }

    #[test]
    fn test_env_similarity_contains_match() {
        let seed = RecommendSeed {
            pattern: "SEEK".to_string(),
            weight: 100,
        };
        assert_eq!(env_similarity_score("DEEPSEEK_API_KEY", &seed), 80);
    }

    #[test]
    fn test_env_similarity_weighted() {
        let seed = RecommendSeed {
            pattern: "DEEPSEEK".to_string(),
            weight: 80,
        };
        // prefix match (90) * weight (80) / 100 = 72
        assert_eq!(env_similarity_score("DEEPSEEK_API_KEY", &seed), 72);
    }

    #[test]
    fn test_env_similarity_no_match() {
        let seed = RecommendSeed {
            pattern: "OPENAI".to_string(),
            weight: 100,
        };
        assert_eq!(env_similarity_score("DEEPSEEK_API_KEY", &seed), 0);
    }

    #[test]
    fn test_apply_recommendations_marks_top3() {
        let mut items = [
            make_item("ANTHROPIC_API_KEY"),
            make_item("DEEPSEEK_API_KEY"),
            make_item("DEEPSEEK_SECRET"),
            make_item("OPENAI_API_KEY"),
            make_item("OTHER_VAR"),
            make_skip(),
        ];

        // Simulate seeds for deepseek product
        let seeds = [
            RecommendSeed {
                pattern: "DEEPSEEK_API_KEY".to_string(),
                weight: 100,
            },
            RecommendSeed {
                pattern: "deepseek".to_string(),
                weight: 80,
            },
        ];

        // Compute scores manually
        let mut scored: Vec<(usize, i32)> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.is_skip)
            .map(|(idx, item)| {
                let max_score = seeds
                    .iter()
                    .map(|seed| env_similarity_score(&item.name, seed))
                    .max()
                    .unwrap_or(0);
                (idx, max_score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        scored.sort_by_key(|a| std::cmp::Reverse(a.1));

        // Mark top 3
        let recommended_indices: Vec<usize> = scored.iter().take(3).map(|(idx, _)| *idx).collect();
        for idx in &recommended_indices {
            items[*idx].recommended = true;
        }

        // DEEPSEEK_API_KEY should be recommended (exact match)
        assert!(
            items
                .iter()
                .find(|i| i.name == "DEEPSEEK_API_KEY")
                .unwrap()
                .recommended
        );
        // OPENAI_API_KEY should not be recommended (no match for deepseek)
        assert!(
            !items
                .iter()
                .find(|i| i.name == "OPENAI_API_KEY")
                .unwrap()
                .recommended
        );
    }

    #[test]
    fn test_apply_recommendations_ordering() {
        let mut items = vec![
            make_item("ANTHROPIC_API_KEY"),
            make_item("DEEPSEEK_API_KEY"),
            make_item("DEEPSEEK_SECRET"),
            make_item("OPENAI_API_KEY"),
            make_item("OTHER_VAR"),
            make_skip(),
        ];

        // Use apply_recommendations with product_id "deepseek"
        apply_recommendations(&mut items, "deepseek");

        // First item should be recommended (DEEPSEEK_API_KEY has exact match)
        assert!(items[0].recommended);
        assert_eq!(items[0].name, "DEEPSEEK_API_KEY");

        // Skip item should be last
        assert!(items.last().unwrap().is_skip);
    }
}
