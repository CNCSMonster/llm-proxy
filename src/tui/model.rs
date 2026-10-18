#![allow(dead_code)] // TUI model fields reserved for future screens

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

/// Entry mode for the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryMode {
    /// `connect` / `provider add` — product selection wizard
    Connect,
    /// `provider` / `provider --select X` — provider list, or detail when `select` is set
    ProviderTui { select: Option<String> },
    /// `model` / `model --select X` — model list, or detail when `select` is set
    ModelTui { select: Option<String> },
    /// `provider login <name>` — start at OAuth login for a specific provider
    OAuthLogin(String),
}

/// Top-level application state.
pub struct AppModel {
    pub config_path: PathBuf,
    pub entry_mode: EntryMode,
    pub screen: Screen,
    /// Accumulated results during a single connect session.
    pub session_results: Vec<ConfigResult>,
    /// Chosen catalog product after ProductSelection.
    pub chosen_product: Option<ProductItem>,
    /// Chosen env var after EnvVarSelection.
    pub chosen_env_var: Option<String>,
    /// Chosen models after ModelSelection.
    pub chosen_models: Vec<String>,
    /// Custom provider fields.
    pub custom_provider_type: Option<String>,
    pub custom_provider_name: Option<String>,
    pub custom_provider_endpoint: Option<String>,
    /// OAuth provider name entered by user
    pub oauth_provider_name: Option<String>,
    /// Whether the current connect flow went through the naming screen
    /// (i.e., this is a repeat connect for the same product).
    /// Controls routing after env selection: true → FallbackConfig, false → ModelSelection.
    pub is_repeat_connect: bool,
    /// Double-ESC protection.
    pub last_esc: Option<Instant>,
    pub esc_hint_visible: bool,
    /// Terminal dimensions.
    pub width: u16,
    pub height: u16,
    /// Cached ModelSelectionState when switching to FallbackConfig,
    /// so that `selected` and cursor position survive round-trips.
    pub cached_model_selection: Option<ModelSelectionState>,
    /// Pending OAuth credential waiting for atomic commit with provider config.
    pub pending_account: Option<crate::auth::PendingAccount>,
    /// Endpoint-level filter from FallbackConfig: model_id → set of protocol
    /// labels ("chat", "responses", "anthropic") the user selected.
    /// After provider creation, bindings for unselected protocols are removed.
    pub endpoint_filter:
        Option<std::collections::HashMap<String, std::collections::HashSet<String>>>,
    /// Fallback target provider name from FallbackConfig (the provider being
    /// backed up). After new provider creation, `add_fallback` inserts the new
    /// provider into the target's model binding chains.
    pub fallback_target: Option<String>,
    /// Fallback binding specs: "model_id:protocol" pairs selected by the user
    /// in FallbackConfig. Passed to `add_fallback` after provider creation.
    pub fallback_bindings: Vec<String>,
    /// OpenRouter 动态目录专用拉取与退避会话状态
    pub openrouter_session: crate::connect::openrouter::OpenRouterConnectSession,
}

impl AppModel {
    pub fn new(config_path: PathBuf, entry_mode: EntryMode) -> Self {
        let products = build_product_list(&config_path);
        let screen = match &entry_mode {
            EntryMode::ProviderTui { select } => {
                let providers = load_configured_providers(&config_path);
                match select.as_deref() {
                    Some(name) => match providers.iter().find(|provider| provider.name == name) {
                        Some(provider) => Screen::ProviderDetail(build_provider_detail(
                            &config_path,
                            provider,
                            None,
                        )),
                        None => Screen::ProviderManagement(ProviderManagementState {
                            providers,
                            cursor: 0,
                            filter: String::new(),
                            filter_active: false,
                            error: Some(format!("unknown provider {name:?}")),
                            scroll_offset: 0,
                            quota_fetched_at: None,
                        }),
                    },
                    None => Screen::ProviderManagement(ProviderManagementState {
                        providers,
                        cursor: 0,
                        filter: String::new(),
                        filter_active: false,
                        error: None,
                        scroll_offset: 0,
                        quota_fetched_at: None,
                    }),
                }
            }
            EntryMode::ModelTui { select } => {
                let models = load_configured_models(&config_path);
                match select.as_deref() {
                    Some(name) => match models.iter().find(|model| model.id == name) {
                        Some(item) => {
                            Screen::ModelDetail(build_model_detail(&config_path, &item.id))
                        }
                        None => Screen::ModelManagement(ModelManagementState {
                            models,
                            cursor: 0,
                            filter: String::new(),
                            filter_active: false,
                            error: Some(format!("unknown model {name:?}")),
                            scroll_offset: 0,
                        }),
                    },
                    None => Screen::ModelManagement(ModelManagementState {
                        models,
                        cursor: 0,
                        filter: String::new(),
                        filter_active: false,
                        error: None,
                        scroll_offset: 0,
                    }),
                }
            }
            EntryMode::OAuthLogin(provider_id) => {
                let config_error = |message: String| {
                    let mut device = OAuthDeviceCodeState::pending_login(provider_id.clone());
                    device.error = Some(message);
                    Screen::OAuthDeviceCode(device)
                };
                let config = match crate::config::Config::load(&config_path) {
                    Ok(config) => config,
                    Err(error) => {
                        return Self::with_initial_screen(
                            config_path,
                            entry_mode.clone(),
                            config_error(format!("Failed to load config: {error}")),
                        );
                    }
                };
                let provider = match config.providers.get(provider_id) {
                    Some(provider) => provider,
                    None => {
                        return Self::with_initial_screen(
                            config_path,
                            entry_mode.clone(),
                            config_error(format!("Unknown provider {provider_id:?}")),
                        );
                    }
                };
                match provider.auth_config(provider_id) {
                    Ok(crate::config::AuthConfig::OpenaiOauth { .. }) => Screen::OAuthDeviceCode(
                        OAuthDeviceCodeState::pending_login(provider_id.clone()),
                    ),
                    Ok(_) => config_error(format!("Provider {provider_id:?} is not OAuth-backed")),
                    Err(error) => {
                        config_error(format!("Failed to load provider {provider_id:?}: {error}"))
                    }
                }
            }
            _ => Screen::ProductSelection(ProductSelectionState {
                items: products,
                cursor: 0,
                filter: String::new(),
                filter_active: false,
                error: None,
            }),
        };
        Self {
            config_path,
            entry_mode,
            screen,
            session_results: Vec::new(),
            chosen_product: None,
            chosen_env_var: None,
            chosen_models: Vec::new(),
            custom_provider_type: None,
            custom_provider_name: None,
            custom_provider_endpoint: None,
            oauth_provider_name: None,
            is_repeat_connect: false,
            last_esc: None,
            esc_hint_visible: false,
            width: 80,
            height: 24,
            cached_model_selection: None,
            pending_account: None,
            endpoint_filter: None,
            fallback_target: None,
            fallback_bindings: Vec::new(),
            openrouter_session: Default::default(),
        }
    }

    fn with_initial_screen(config_path: PathBuf, entry_mode: EntryMode, screen: Screen) -> Self {
        Self {
            config_path,
            entry_mode,
            screen,
            session_results: Vec::new(),
            chosen_product: None,
            chosen_env_var: None,
            chosen_models: Vec::new(),
            custom_provider_type: None,
            custom_provider_name: None,
            custom_provider_endpoint: None,
            oauth_provider_name: None,
            is_repeat_connect: false,
            last_esc: None,
            esc_hint_visible: false,
            width: 80,
            height: 24,
            cached_model_selection: None,
            pending_account: None,
            endpoint_filter: None,
            fallback_target: None,
            fallback_bindings: Vec::new(),
            openrouter_session: Default::default(),
        }
    }

    /// Return the current error message, if any.
    pub fn current_error(&self) -> Option<&str> {
        match &self.screen {
            Screen::ProductSelection(s) => s.error.as_deref(),
            Screen::EnvVarSelection(s) => s.error.as_deref(),
            Screen::ModelSelection(s) => s.error.as_deref(),
            Screen::CustomProviderEditor(s) => s.error.as_deref(),
            Screen::ProviderNaming(_) => None,
            Screen::OAuthName(s) => s.error.as_deref(),
            Screen::OAuthDeviceCode(s) => s.error.as_deref(),
            Screen::WarningConfirm(s) => s.error.as_deref(),
            Screen::ResetUsageConfirm(s) => s.error.as_deref(),
            Screen::FallbackConfig(s) => s.error.as_deref(),
            Screen::CopyModelConfirm(s) => s.error.as_deref(),
            Screen::CopyConfigure(s) => s.error.as_deref(),
            Screen::ModelRename(s) => s.error.as_deref(),
            Screen::ModelManagement(s) => s.error.as_deref(),
            Screen::ModelDetail(s) => s.error.as_deref(),
            Screen::ModelDeleteConfirm(s) => s.error.as_deref(),
            Screen::ProtocolBindingEditor(s) => s.error.as_deref(),
            _ => None,
        }
    }

    /// Return the help text for the current screen.
    pub fn help_text(&self) -> &'static str {
        match &self.screen {
            Screen::ProviderManagement(s) => {
                if s.filter_active {
                    "[Esc] Cancel  [Enter] Confirm"
                } else {
                    "[/] Search  [a] Add  [c] Copy  [m] Models  [e] Edit  [Enter] Detail  [q] Quit\n[d] Delete  [f] Fallback  [l] Login  [u] Usage  [R] Reset\n[r] Refresh  [W] Quota"
                }
            }
            Screen::ProviderDetail(s) => {
                if s.bound_models.is_empty() {
                    "[c] Copy  [m] Models  [e] Edit  [d] Delete  [Esc] Back"
                } else {
                    "[↑/↓] Select Model  [c] Copy  [m] Models  [r] Rename  [e] Edit  [d] Delete  [Esc] Back"
                }
            }
            Screen::DeleteConfirm(_) => "[y] Confirm  [n] Cancel  [f] Force Delete",
            Screen::ResetUsageConfirm(_) => "[y] Confirm Reset  [n] Cancel",
            Screen::ProductSelection(_) => {
                "[/] Search  [↑/↓/j/k] Move  [Enter] Confirm  [Esc] Back  [q] Quit"
            }
            Screen::EnvVarSelection(_) => {
                "[/] Search  [↑/↓/j/k] Move  [Enter] Confirm  [s] Skip  [Esc] Back"
            }
            Screen::ModelSelection(_) => {
                "[Space] Toggle  [a] All  [c] Copy  [↑/↓/j/k] Move  [Enter] Confirm  [F] Fallback  [Esc] Back"
            }
            Screen::CustomProviderEditor(_) => "[↑/↓/j/k] Move  [Enter] Edit  [Esc] Save",
            Screen::ProviderNaming(s) => {
                if s.editing {
                    "[Esc] Cancel edit  [Enter] Confirm"
                } else {
                    "[e] Edit  [Enter] Confirm  [Esc] Back"
                }
            }
            Screen::ProbeConfirm(_) => "[↑/↓/j/k] Move  [Enter] Confirm  [n] Skip  [Esc] Back",
            Screen::Verifying(_) => "Verifying... please wait",
            Screen::WarningConfirm(_) => "[↑/↓/j/k] Move  [Enter] Confirm",
            Screen::Done(_) => "[Enter] Add Another  [q] Quit",
            Screen::OAuthName(_) => "[↑/↓/j/k] Move  [Enter] Confirm  [Esc] Back",
            Screen::OAuthDeviceCode(_) => "[r/Enter] 重试登录  [s] 停止  [Esc] 返回",
            Screen::FallbackConfig(s) => {
                if s.focus == FallbackFocus::TargetProvider {
                    "[↑/↓/j/k] Move  [Tab] Switch to options  [Enter] Confirm  [M] Models  [Esc] Skip"
                } else {
                    "[↑/↓/j/k] Move  [Space] Toggle  [Tab] Switch to targets  [Enter] Confirm  [Esc] Back"
                }
            }
            Screen::OAuthOverwrite(_) => "[↑/↓/j/k] Move  [Enter] Confirm",
            Screen::CopyModelConfirm(s) => {
                if s.editing {
                    "[Esc] Cancel edit  [Enter] Confirm copy"
                } else {
                    "[e] Edit ID  [Enter] Confirm copy  [Esc] Back"
                }
            }
            Screen::CopyConfigure(s) => match s.step {
                CopyConfigureStep::Name if s.editing => "[Enter] Next  [Esc] Cancel edit",
                CopyConfigureStep::Name => "[e] Edit  [Enter] Next  [Esc] Back",
                CopyConfigureStep::Auth if s.env_editing => "[Enter] Confirm  [Esc] Cancel edit",
                CopyConfigureStep::Auth => match s.auth_kind {
                    CopyAuthKind::ApiKey => {
                        "[e] Edit env  [s] No API key  [Enter] Confirm copy  [Esc] Back"
                    }
                    CopyAuthKind::None => "[Enter] Confirm copy  [Esc] Back",
                    CopyAuthKind::OpenaiOauth => "[Enter] Copy and login  [Esc] Back",
                },
            },
            Screen::ModelRename(s) => {
                if s.confirm_step == 0 {
                    if s.editing {
                        "[Enter] Next  [Esc] Cancel"
                    } else {
                        "[e] Edit  [Enter] Next  [Esc] Back"
                    }
                } else {
                    "[y] Confirm Rename  [n] Cancel"
                }
            }
            Screen::ModelManagement(s) => {
                if s.filter_active {
                    "[Esc] Cancel  [Enter] Confirm"
                } else {
                    "[/] Search  [a] Add  [c] Copy  [r] Rename  [Enter] Detail  [q] Quit\n[d] Delete"
                }
            }
            Screen::ModelDetail(s) => {
                if s.editing {
                    "[Enter] Confirm  [Esc] Cancel edit"
                } else if s.is_draft() {
                    "[↑/↓] Field  [Enter] Edit  [s] Save  [Esc] Cancel"
                } else if s.is_routing_model {
                    "[↑/↓] Field  [r] Rename  [Esc] Back"
                } else {
                    "[↑/↓] Field  [Enter] Edit  [r] Rename  [Esc] Back"
                }
            }
            Screen::ModelDeleteConfirm(_) => "[y] Confirm  [n] Cancel",
            Screen::ProtocolBindingEditor(s) => {
                if s.remove_confirm {
                    "[y] Remove  [n] Cancel"
                } else if s.editing_upstream {
                    "[Enter] Confirm  [Esc] Cancel edit"
                } else if s.adding {
                    "[↑/↓] Provider  [Enter] Add  [Esc] Cancel"
                } else {
                    "[a] Add  [e] Upstream  [K/J] Move  [d] Remove  [s] Save  [Esc] Back"
                }
            }
            Screen::Quit => "",
        }
    }
}

// ── Screen Enum ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Screen {
    /// Provider 管理面板（主视图）
    ProviderManagement(ProviderManagementState),
    /// Provider 详情视图
    ProviderDetail(ProviderDetailState),
    /// 删除确认对话框
    DeleteConfirm(DeleteConfirmState),
    /// 重置 usage 确认对话框
    ResetUsageConfirm(ResetUsageConfirmState),
    ProductSelection(ProductSelectionState),
    EnvVarSelection(EnvVarSelectionState),
    ModelSelection(ModelSelectionState),
    /// 探测确认屏：模型确认后、网络探测前，由用户选择是否探测（默认高亮「探测」）
    ProbeConfirm(ProbeConfirmState),
    CustomProviderEditor(CustomProviderEditorState),
    ProviderNaming(ProviderNamingState),
    OAuthName(OAuthNameState),
    OAuthDeviceCode(OAuthDeviceCodeState),
    WarningConfirm(WarningConfirmState),
    Verifying(VerifyingState),
    OAuthOverwrite(OAuthOverwriteState),
    Done(DoneState),
    /// Fallback 配置界面（重复 connect 时，env 选择后可进入）
    FallbackConfig(FallbackConfigState),
    /// Model 复制确认界面
    CopyModelConfirm(CopyModelConfirmState),
    /// Provider 复制配置界面
    CopyConfigure(CopyConfigureState),
    /// Model 重命名界面
    ModelRename(ModelRenameState),
    /// Model 管理主列表
    ModelManagement(ModelManagementState),
    /// Model 详情 / 新增 / 复制草稿
    ModelDetail(ModelDetailState),
    /// 删除模型确认
    ModelDeleteConfirm(ModelDeleteConfirmState),
    /// 单协议 Provider 绑定编辑器
    ProtocolBindingEditor(ProtocolBindingEditorState),
    Quit,
}

// ── State Structs ────────────────────────────────────────────────────────

/// Provider 认证类型（互斥枚举）
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAuthType {
    ApiKey,
    OpenaiOauth,
    None,
}

impl ProviderAuthType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApiKey => "API Key",
            Self::OpenaiOauth => "OAuth",
            Self::None => "None",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProductItem {
    pub id: String,
    pub display_name: String,
    pub auth_type: ProviderAuthType,
    pub endpoint_count: usize,
    pub product_kind: String, // "payg", "subscription", "local", "custom"
    pub is_custom: bool,
}

// ── Provider Management States ───────────────────────────────────────────

/// Provider 列表项（主面板显示用）
#[derive(Debug, Clone)]
pub struct ProviderListItem {
    /// Provider 实例名（用户自定义，如 "my-kimi"）
    pub name: String,
    /// 产品名（catalog product ID 或 "Custom"）
    pub product: String,
    /// 认证类型显示文本
    pub auth_type: String,
    /// 状态：Ok / Warning / Error
    pub status: ProviderStatus,
    /// 支持的协议列表
    pub protocols: Vec<String>,
    /// 冷却状态（若存在任何未过期冷却，格式如 "until 2026-10-01 12:00"）
    pub cooldown: Option<String>,
}

/// Provider 状态
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderStatus {
    Ok,
    Warning,
    Error,
}

/// Provider 管理列表的可视行数：终端高度扣除外边框(2)、帮助栏(2)、内边框(2)、表头(1)。
pub fn provider_list_viewport(app_height: u16) -> usize {
    app_height.saturating_sub(7) as usize
}

/// Provider 管理面板状态
#[derive(Debug, Clone)]
pub struct ProviderManagementState {
    pub providers: Vec<ProviderListItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
    /// 列表滚动窗口首行在 filtered_providers 中的下标，保证 cursor 始终可见。
    pub scroll_offset: usize,
    /// 上次配额刷新时间
    pub quota_fetched_at: Option<std::time::SystemTime>,
}

impl ProviderManagementState {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.clear_success_message();
    }

    pub fn move_down(&mut self) {
        if self.cursor < self.filtered_count().saturating_sub(1) {
            self.cursor += 1;
        }
        self.clear_success_message();
    }

    /// error 消息固定渲染在列表底部时占用的行数。
    pub fn error_row_count(&self) -> usize {
        usize::from(self.error.is_some())
    }

    /// 调整滚动窗口，使 cursor 始终落在可视区域内。
    pub fn ensure_cursor_visible(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset.saturating_add(viewport) {
            self.scroll_offset = self.cursor.saturating_add(1).saturating_sub(viewport);
        }
    }

    /// 清除成功消息（以 "✓" 开头），保留错误消息供用户排查。
    fn clear_success_message(&mut self) {
        if self.error.as_deref().is_some_and(|e| e.starts_with('✓')) {
            self.error = None;
        }
    }

    pub fn selected_provider(&self) -> Option<&ProviderListItem> {
        self.filtered_providers().get(self.cursor).copied()
    }

    pub fn filtered_providers(&self) -> Vec<&ProviderListItem> {
        if self.filter.is_empty() {
            self.providers.iter().collect()
        } else {
            let filter_lower = self.filter.to_lowercase();
            self.providers
                .iter()
                .filter(|p| {
                    p.name.to_lowercase().contains(&filter_lower)
                        || p.product.to_lowercase().contains(&filter_lower)
                })
                .collect()
        }
    }

    pub fn filtered_count(&self) -> usize {
        self.filtered_providers().len()
    }

    pub fn activate_filter(&mut self) {
        self.filter_active = true;
    }

    pub fn deactivate_filter(&mut self) {
        self.filter_active = false;
        self.filter.clear();
        self.cursor = 0;
    }
}

/// Provider 详情视图状态
#[derive(Debug, Clone)]
pub struct ProviderDetailState {
    /// Provider 实例名
    pub name: String,
    /// 产品名
    pub product: String,
    /// 认证方式描述
    pub auth_description: String,
    /// 认证状态（已配置/未配置等）
    pub auth_status: String,
    /// Endpoints 列表
    pub endpoints: Vec<EndpointInfo>,
    /// 绑定的模型列表
    pub bound_models: Vec<String>,
    /// Compat 信息
    pub compat_info: Vec<String>,
    /// 当前选中的模型光标（用于模型管理操作）
    pub model_cursor: usize,
    /// 绑定模型列表的滚动窗口首行下标（"绑定模型"表头占第 0 行）
    pub scroll_offset: usize,
    /// 配额数据获取时间
    pub quota_fetched_at: Option<std::time::SystemTime>,
}

impl ProviderDetailState {
    /// Move model cursor up.
    pub fn model_move_up(&mut self) {
        self.model_cursor = self.model_cursor.saturating_sub(1);
    }

    /// Move model cursor down.
    pub fn model_move_down(&mut self) {
        let max = self.bound_models.len().saturating_sub(1);
        if self.model_cursor < max {
            self.model_cursor += 1;
        }
    }

    /// Get the currently selected model ID, if any.
    pub fn selected_model(&self) -> Option<&str> {
        self.bound_models.get(self.model_cursor).map(|s| s.as_str())
    }

    /// 光标行在绑定模型列表内容中的下标（含表头行）。
    pub fn cursor_line_index(&self) -> usize {
        1 + self.model_cursor
    }
}

/// Model rename 确认对话框状态
#[derive(Debug, Clone)]
pub struct ModelRenameState {
    /// 原始 model ID
    pub old_model_id: String,
    /// 用户输入的新 model ID
    pub new_model_id: String,
    /// 是否处于编辑模式
    pub editing: bool,
    /// 输入光标位置
    pub cursor_pos: usize,
    /// 确认步骤：0 = 输入新 ID，1 = 二次确认
    pub confirm_step: u8,
    /// 错误/状态消息
    pub error: Option<String>,
    /// 触发重命名的 provider 名称（用于返回 ProviderDetail）
    pub provider_name: String,
    /// 从 Model TUI 进入时，完成后回到模型列表
    pub return_to_model_list: bool,
}

/// Endpoint 信息
#[derive(Debug, Clone)]
pub struct EndpointInfo {
    pub protocol: String,
    pub url: String,
    pub kind: String, // "native" or "derived"
}

/// 删除确认对话框状态
#[derive(Debug, Clone)]
pub struct DeleteConfirmState {
    /// 要删除的 provider 名称
    pub provider_name: String,
    /// 引用该 provider 的模型列表
    pub referencing_models: Vec<String>,
    /// 是否强制删除模式
    pub force_mode: bool,
    /// 删除失败时的错误信息
    pub error: Option<String>,
}

/// 重置 usage 确认对话框状态
#[derive(Debug, Clone)]
pub struct ResetUsageConfirmState {
    /// 要重置 usage 的 provider 名称
    pub provider_name: String,
    /// Usage 查询结果摘要（None = 正在加载）
    pub usage_info: Option<String>,
    /// 可用 reset credit 数量
    pub credits: Option<i64>,
    /// 操作失败时的错误信息
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProductSelectionState {
    pub items: Vec<ProductItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
}

impl ProductSelectionState {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn move_down(&mut self) {
        let max = self.filtered_count().saturating_sub(1);
        if self.cursor < max {
            self.cursor += 1;
        }
    }
    pub fn activate_filter(&mut self) {
        self.filter_active = true;
    }
    pub fn deactivate_filter(&mut self) {
        self.filter_active = false;
        self.filter.clear();
    }
    /// 计数/下标走共享的 filtered_len/filtered_item_at，谓词与 widgets::product_list
    /// 一致（product_filter_pred），保证状态层与渲染层是同一集合。
    fn filtered_count(&self) -> usize {
        filtered_len(&self.items, &self.filter, product_filter_pred)
    }
    pub fn filtered_item(&self) -> Option<&ProductItem> {
        filtered_item_at(&self.items, &self.filter, self.cursor, product_filter_pred)
    }
}

#[derive(Debug, Clone)]
pub struct EnvItem {
    pub name: String,
    pub is_skip: bool,
    pub recommended: bool,
}

#[derive(Debug, Clone)]
pub struct EnvVarSelectionState {
    pub items: Vec<EnvItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
    pub product_name: String,
    /// Warning when the currently highlighted env var is already used by another provider.
    /// Format: "该 key 已被 {provider_name} 使用"
    pub env_in_use_warning: Option<String>,
}

impl EnvVarSelectionState {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn move_down(&mut self) {
        let max = self.filtered_count().saturating_sub(1);
        if self.cursor < max {
            self.cursor += 1;
        }
    }
    pub fn activate_filter(&mut self) {
        self.filter_active = true;
    }
    pub fn deactivate_filter(&mut self) {
        self.filter_active = false;
        self.filter.clear();
    }
    /// 计数/下标走共享的 filtered_len/filtered_item_at，谓词与 widgets::env_list
    /// 一致（env_filter_pred：skip 项仅在无过滤时计入）。
    fn filtered_count(&self) -> usize {
        filtered_len(&self.items, &self.filter, env_filter_pred)
    }
    pub fn filtered_item(&self) -> Option<&EnvItem> {
        filtered_item_at(&self.items, &self.filter, self.cursor, env_filter_pred)
    }
}

#[derive(Debug, Clone)]
pub struct ModelItem {
    pub id: String,
    pub display_name: String,
    /// 上下文窗口。`None` = 目录未声明（宁缺毋假，不虚构默认值）。
    pub context_window: Option<i64>,
    /// 最大输出 token。`None` = 目录未声明。
    pub max_output_tokens: Option<i64>,
    /// 目录声明的能力（如 `image_input` / `tools`），视觉能力从中推导。
    pub features: Vec<String>,
    /// 目录声明的思考档位；空 = 未声明思考能力。
    pub reasoning_levels: Vec<String>,
    /// 目录声明的默认思考档位；仅当命中 `reasoning_levels` 时有意义。
    pub default_reasoning_level: Option<String>,
}

impl ModelItem {
    pub fn supports_image(&self) -> bool {
        self.features.iter().any(|f| f == "image_input")
    }

    /// 有声明思考档位则展示思考标识。
    pub fn has_thinking(&self) -> bool {
        !self.reasoning_levels.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelSelectionSource {
    Connect,
    ProviderReselect { provider_name: String },
}

#[derive(Debug, Clone)]
pub struct ModelSelectionState {
    pub items: Vec<ModelItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub selected: HashSet<String>,
    pub configured: HashSet<String>,
    pub error: Option<String>,
    pub product_name: String,
    pub source: ModelSelectionSource,
    pub is_openrouter: bool,
    pub is_degraded: bool,
    pub openrouter_cached_at: Option<i64>,
}

impl ModelSelectionState {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn move_down(&mut self) {
        let max = self.filtered_count().saturating_sub(1);
        if self.cursor < max {
            self.cursor += 1;
        }
    }
    pub fn activate_filter(&mut self) {
        self.filter_active = true;
    }
    pub fn deactivate_filter(&mut self) {
        self.filter_active = false;
        self.filter.clear();
    }
    /// 计数/下标走共享的 filtered_len/filtered_item_at，谓词与 widgets::model_list
    /// 一致（model_filter_pred）。
    fn filtered_count(&self) -> usize {
        filtered_len(&self.items, &self.filter, model_filter_pred)
    }
    pub fn filtered_item(&self) -> Option<&ModelItem> {
        filtered_item_at(&self.items, &self.filter, self.cursor, model_filter_pred)
    }
}

// ── Fallback Config State ────────────────────────────────────────────────

/// A single selectable item in the fallback config screen.
/// Each item represents a (model, endpoint) combination.
#[derive(Debug, Clone)]
pub struct FallbackOption {
    /// Model ID (e.g. "deepseek-v4-flash-lp")
    pub model_id: String,
    /// Display name (e.g. "deepseek-v4-flash")
    pub model_display_name: String,
    /// Endpoint protocol (e.g. "chat", "responses", "anthropic")
    pub endpoint: String,
    /// Whether this option is selected by the user
    pub selected: bool,
}

/// State for the fallback configuration screen.
/// Shown after env selection when doing a repeat connect,
/// or from the provider management panel (press 'f').
#[derive(Debug, Clone)]
pub struct FallbackConfigState {
    /// Other providers of the same product that can be fallback targets
    pub target_providers: Vec<String>,
    /// Cursor position in the target provider list (-1 means no target selected)
    pub target_cursor: usize,
    /// Combined (model, endpoint) options for multi-select
    pub options: Vec<FallbackOption>,
    /// Cursor position in the options list
    pub option_cursor: usize,
    /// Which section has focus: target selection or options selection
    pub focus: FallbackFocus,
    /// Error message (if any)
    pub error: Option<String>,
    /// Whether the user chose to skip fallback (proceed directly to done)
    pub skipped: bool,
    /// Where this screen was entered from (controls Esc/Enter behavior)
    pub source: FallbackSource,
    /// 主内容区滚动窗口首行下标（目标 Provider 区 + options 区）
    pub scroll_offset: usize,
}

/// Where the FallbackConfig screen was entered from.
#[derive(Debug, Clone, PartialEq)]
pub enum FallbackSource {
    /// Entered from the connect flow (repeat connect after env selection).
    /// Enter → Verifying, Esc → skip to Verifying.
    ConnectFlow,
    /// Entered from the provider management panel (press 'f').
    /// `provider_name` is the source provider (the fallback provider / 兜底者).
    /// Enter → call batch fallback + return to provider list.
    /// Esc → return to provider list.
    ProviderManagement { provider_name: String },
}

/// Which section has focus in the fallback config screen.
#[derive(Debug, Clone, PartialEq)]
pub enum FallbackFocus {
    TargetProvider,
    Options,
}

impl FallbackConfigState {
    fn targets_block_lines(&self) -> usize {
        if self.target_providers.is_empty() {
            1 // "(无同产品的其他 Provider)" 占位行
        } else {
            self.target_providers.len()
        }
    }

    /// 当前焦点光标在内容行中的下标（目标 Provider 区或 options 区）。
    pub fn active_cursor_line(&self) -> usize {
        match self.focus {
            FallbackFocus::TargetProvider => self.target_cursor,
            FallbackFocus::Options => self.targets_block_lines() + 2 + self.option_cursor,
        }
    }

    pub fn move_up(&mut self) {
        match self.focus {
            FallbackFocus::TargetProvider => {
                self.target_cursor = self.target_cursor.saturating_sub(1);
            }
            FallbackFocus::Options => {
                self.option_cursor = self.option_cursor.saturating_sub(1);
            }
        }
    }

    pub fn move_down(&mut self) {
        match self.focus {
            FallbackFocus::TargetProvider => {
                let max = self.target_providers.len().saturating_sub(1);
                if self.target_cursor < max {
                    self.target_cursor += 1;
                }
            }
            FallbackFocus::Options => {
                let max = self.options.len().saturating_sub(1);
                if self.option_cursor < max {
                    self.option_cursor += 1;
                }
            }
        }
    }

    pub fn toggle_option(&mut self) {
        if let Some(opt) = self.options.get_mut(self.option_cursor) {
            opt.selected = !opt.selected;
        }
    }

    pub fn selected_options(&self) -> Vec<&FallbackOption> {
        self.options.iter().filter(|o| o.selected).collect()
    }

    pub fn selected_target(&self) -> Option<&str> {
        self.target_providers
            .get(self.target_cursor)
            .map(|s| s.as_str())
    }
}

/// Fields available in the custom provider editor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderField {
    Name,
    ApiKeyEnv,
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

/// Single field state in the custom provider editor.
#[derive(Debug, Clone)]
pub struct ProviderFieldState {
    pub field: ProviderField,
    pub label: &'static str,
    pub value: String,
    pub placeholder: &'static str,
}

/// 编辑器的用途：创建新 provider 或编辑已有 provider
#[derive(Debug, Clone, PartialEq)]
pub enum EditorPurpose {
    /// 创建新 provider
    Create,
    /// 编辑已有 provider
    Edit {
        /// 原始名称（用于检测名称变更）
        original_name: String,
        /// 认证类型（只读显示）
        auth_type: String,
    },
}

/// Editor mode: browsing the field list or editing a specific field.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorMode {
    Browse,
    Edit,
}

/// Which area has focus in the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorFocus {
    Fields,
    Buttons,
}

/// Button in the editor button bar.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorButton {
    Save,
    Cancel,
}

/// State for the custom provider multi-field editor screen.
#[derive(Debug, Clone)]
pub struct CustomProviderEditorState {
    pub fields: Vec<ProviderFieldState>,
    pub cursor: usize,
    pub mode: EditorMode,
    pub purpose: EditorPurpose,
    pub editable_fields: std::collections::HashSet<ProviderField>,
    pub edit_cursor: usize,
    pub focus: EditorFocus,
    pub button_cursor: usize,
    pub error: Option<String>,
}

impl CustomProviderEditorState {
    pub fn new() -> Self {
        use std::collections::HashSet;
        let mut editable_fields = HashSet::new();
        editable_fields.insert(ProviderField::Name);
        editable_fields.insert(ProviderField::ApiKeyEnv);
        editable_fields.insert(ProviderField::OpenAiChat);
        editable_fields.insert(ProviderField::OpenAiResponses);
        editable_fields.insert(ProviderField::Anthropic);

        Self {
            fields: vec![
                ProviderFieldState {
                    field: ProviderField::Name,
                    label: "Name",
                    value: String::new(),
                    placeholder: "my-provider",
                },
                ProviderFieldState {
                    field: ProviderField::ApiKeyEnv,
                    label: "API Key Env",
                    value: String::new(),
                    placeholder: "MY_API_KEY",
                },
                ProviderFieldState {
                    field: ProviderField::OpenAiChat,
                    label: "OpenAI Chat",
                    value: String::new(),
                    placeholder: "https://api.example.com/v1/chat/completions",
                },
                ProviderFieldState {
                    field: ProviderField::OpenAiResponses,
                    label: "OpenAI Resp",
                    value: String::new(),
                    placeholder: "https://api.example.com/v1/responses",
                },
                ProviderFieldState {
                    field: ProviderField::Anthropic,
                    label: "Anthropic",
                    value: String::new(),
                    placeholder: "https://api.example.com/v1/messages",
                },
            ],
            cursor: 0,
            mode: EditorMode::Browse,
            purpose: EditorPurpose::Create,
            editable_fields,
            edit_cursor: 0,
            focus: EditorFocus::Fields,
            button_cursor: 0,
            error: None,
        }
    }

    /// 创建编辑已有 provider 的编辑器状态
    pub fn new_edit(
        provider_name: &str,
        auth_type: &str,
        api_key_env: Option<&str>,
        openai_chat_url: Option<&str>,
        openai_responses_url: Option<&str>,
        anthropic_url: Option<&str>,
    ) -> Self {
        use std::collections::HashSet;
        let mut editable_fields = HashSet::new();
        // 编辑模式下，Name、ApiKeyEnv 和所有 Endpoints 都可编辑
        editable_fields.insert(ProviderField::Name);
        editable_fields.insert(ProviderField::ApiKeyEnv);
        editable_fields.insert(ProviderField::OpenAiChat);
        editable_fields.insert(ProviderField::OpenAiResponses);
        editable_fields.insert(ProviderField::Anthropic);

        Self {
            fields: vec![
                ProviderFieldState {
                    field: ProviderField::Name,
                    label: "Name",
                    value: provider_name.to_string(),
                    placeholder: "my-provider",
                },
                ProviderFieldState {
                    field: ProviderField::ApiKeyEnv,
                    label: "API Key Env",
                    value: api_key_env.unwrap_or("").to_string(),
                    placeholder: "MY_API_KEY",
                },
                ProviderFieldState {
                    field: ProviderField::OpenAiChat,
                    label: "OpenAI Chat",
                    value: openai_chat_url.unwrap_or("").to_string(),
                    placeholder: "https://api.example.com/v1/chat/completions",
                },
                ProviderFieldState {
                    field: ProviderField::OpenAiResponses,
                    label: "OpenAI Resp",
                    value: openai_responses_url.unwrap_or("").to_string(),
                    placeholder: "https://api.example.com/v1/responses",
                },
                ProviderFieldState {
                    field: ProviderField::Anthropic,
                    label: "Anthropic",
                    value: anthropic_url.unwrap_or("").to_string(),
                    placeholder: "https://api.example.com/v1/messages",
                },
            ],
            cursor: 0,
            mode: EditorMode::Browse,
            purpose: EditorPurpose::Edit {
                original_name: provider_name.to_string(),
                auth_type: auth_type.to_string(),
            },
            editable_fields,
            edit_cursor: 0,
            focus: EditorFocus::Fields,
            button_cursor: 0,
            error: None,
        }
    }

    /// 检查字段是否可编辑
    pub fn is_field_editable(&self, field: ProviderField) -> bool {
        self.editable_fields.contains(&field)
    }

    /// Returns true if at least one endpoint field has a value.
    pub fn has_endpoint(&self) -> bool {
        self.fields.iter().any(|f| {
            matches!(
                f.field,
                ProviderField::OpenAiChat
                    | ProviderField::OpenAiResponses
                    | ProviderField::Anthropic
            ) && !f.value.is_empty()
        })
    }

    /// Returns the current field being edited.
    pub fn current_field(&self) -> &ProviderFieldState {
        &self.fields[self.cursor]
    }

    /// Returns the current field being edited (mutable).
    pub fn current_field_mut(&mut self) -> &mut ProviderFieldState {
        &mut self.fields[self.cursor]
    }
}

/// State for the provider naming screen (shown on 2nd+ connect of same product).
#[derive(Debug, Clone)]
pub struct ProviderNamingState {
    /// Product ID (e.g. "deepseek")
    pub product_id: String,
    /// Product display name (e.g. "DeepSeek API")
    pub product_display_name: String,
    /// Pre-filled default name (e.g. "deepseek-2")
    pub default_name: String,
    /// Current input value
    pub input: String,
    /// Whether editing mode is active (cursor visible, accepting input)
    pub editing: bool,
    /// Cursor position within the input string
    pub cursor_pos: usize,
    /// Validation message (e.g. "名字可用" or "名字已被占用")
    pub validation: NameValidation,
    /// The product item (kept for continuing the flow after naming)
    pub product_item: ProductItem,
}

/// Validation result for provider name.
#[derive(Debug, Clone, PartialEq)]
pub enum NameValidation {
    /// Name is available
    Available,
    /// Name conflicts with an existing provider
    Conflict,
}

#[derive(Debug, Clone)]
pub struct OAuthNameState {
    pub recommended_name: String,
    pub selected_option: NameOption,
    pub input: String,
    pub input_active: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum NameOption {
    Recommended,
    Custom,
}

#[derive(Debug, Clone)]
pub struct OAuthDeviceCodeState {
    pub provider_name: String,
    pub device_code: String,
    pub verification_url: String,
    pub expires_at: Instant,
    pub copied: bool,
    pub polling: bool,
    pub retry_count: u32,
    pub last_error: Option<String>,
    pub error: Option<String>,
}

impl OAuthDeviceCodeState {
    /// 构造"等待触发登录"的初始状态：空设备码 + 未轮询，主循环据此挂起 TUI
    /// 调起真实 CLI 登录。统一构造入口，避免多处手工构造漂移出死状态
    /// （历史上两处构造出假码 + polling=true 的占位，永远无法触发登录）。
    pub fn pending_login(provider_name: String) -> Self {
        Self {
            provider_name,
            device_code: String::new(),
            verification_url: String::new(),
            expires_at: Instant::now() + std::time::Duration::from_secs(15 * 60),
            copied: false,
            polling: false,
            retry_count: 0,
            last_error: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WarningConfirmState {
    pub message: String,
    pub selected_option: WarningOption,
    pub back_to_product: bool, // true → Back 返回产品列表；false → 返回 EnvVarSelection
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum WarningOption {
    Continue,
    Back,
}

/// 探测确认屏状态（ 方案 A）。
///
/// 位置：**本地 env var 存在性检查之后、网络有效性探测之前**。
/// 两道检查正交——`require_auth_env` 管本地凭据，`skip_probe` 只管花钱的网络探测；
/// 跳过探测不代表跳过凭据检查。
#[derive(Debug, Clone)]
pub struct ProbeConfirmState {
    pub product_name: String,
    pub models: Vec<String>,
    /// 0 = 探测（默认），1 = 跳过探测
    pub cursor: usize,
}

impl ProbeConfirmState {
    pub fn probe_selected(&self) -> bool {
        self.cursor == 0
    }
}

#[derive(Debug, Clone)]
pub struct VerifyingState {
    pub product_name: String,
    pub env_var: Option<String>,
    pub models: Vec<String>,
    pub skip_probe: bool, // true → Continue 时跳过探针验证并统一调度
    pub progress_done: usize,
    pub progress_total: usize,
    pub progress_current_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OAuthOverwriteState {
    pub provider_name: String,
    pub account_email: String,
    pub selected_option: WarningOption,
}

#[derive(Debug, Clone)]
pub struct DoneState {
    pub results: Vec<ConfigResult>,
}

#[derive(Debug, Clone)]
pub struct ConfigResult {
    pub provider: String,
    pub success: bool,
    pub message: String,
}

// ── Copy Provider Configuration State ───────────────────────────────────

/// Steps in the provider-copy configuration screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyConfigureStep {
    /// Choose the new provider instance name.
    Name,
    /// Confirm API-key env, skip key, or acknowledge OAuth fresh-account login.
    Auth,
}

/// Auth kind of the source provider being copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyAuthKind {
    ApiKey,
    OpenaiOauth,
    None,
}

/// State for the provider-copy configuration screen.
/// Shown when the user presses `c` on a provider in the management list.
#[derive(Debug, Clone)]
pub struct CopyConfigureState {
    pub source_id: String,
    pub source_product: String,
    pub source_auth_label: String,
    pub source_endpoints: String,
    pub auth_kind: CopyAuthKind,
    pub default_name: String,
    pub input: String,
    pub editing: bool,
    pub cursor_pos: usize,
    pub validation: NameValidation,
    pub step: CopyConfigureStep,
    /// Source provider's existing env, if any. Shown for confirmation only.
    pub source_api_key_env: Option<String>,
    /// User-confirmed env var name. Empty until the user types or confirms.
    pub env_input: String,
    pub env_editing: bool,
    pub env_cursor_pos: usize,
    pub error: Option<String>,
}

// ── Copy Model Confirmation State ───────────────────────────────────────

/// State for the copy-model confirmation screen.
/// Shown when the user presses 'c' on a model in the ModelSelection screen.
#[derive(Debug, Clone)]
pub struct CopyModelConfirmState {
    /// Source model ID being copied
    pub source_model_id: String,
    /// Default new model ID (e.g. "deepseek-v4-flash-lp-1")
    pub default_new_id: String,
    /// User-edited new model ID
    pub new_id: String,
    /// Cursor position within the new_id input
    pub cursor_pos: usize,
    /// Whether editing mode is active (cursor visible, accepting input)
    pub editing: bool,
    /// Validation: whether the new_id is available
    pub validation: NameValidation,
    /// Error message if copy fails
    pub error: Option<String>,
    /// The product name for display
    pub product_name: String,
}

/// Generate a default new model ID for copying, following the pattern `SOURCE-N`.
/// Increments N until an unused ID is found.
pub fn generate_default_copy_id(
    source_id: &str,
    existing_models: &std::collections::BTreeMap<String, crate::config::ModelConfig>,
) -> String {
    for n in 1..=999 {
        let candidate = format!("{}-{}", source_id, n);
        if !existing_models.contains_key(&candidate) {
            return candidate;
        }
    }
    // Fallback: unlikely to reach here
    format!("{}-copy", source_id)
}

#[derive(Debug, Clone)]
pub struct ModelListItem {
    pub id: String,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub thinking: String,
    pub features: String,
    pub provider_count: usize,
    /// Non-zero when this is a routing model, whose children are its
    /// routes rather than provider bindings.
    pub route_count: usize,
}

#[derive(Debug, Clone)]
pub struct ModelManagementState {
    pub models: Vec<ModelListItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
    /// 列表滚动窗口首行在 filtered_models 中的下标，保证 cursor 始终可见。
    pub scroll_offset: usize,
}

/// Model 管理列表的可视行数：终端高度扣除外边框(2)、帮助栏(2)、内边框(2)、表头(1)。
pub fn model_list_viewport(app_height: u16) -> usize {
    app_height.saturating_sub(7) as usize
}

/// 详情/编辑器类屏幕的可视行数：终端高度扣除外边框(2)、帮助栏(1)、内边框(2)。
pub fn detail_viewport(app_height: u16) -> usize {
    app_height.saturating_sub(5) as usize
}

/// 通用滚动窗口跟随：保证 cursor_line 始终落在 [offset, offset+viewport) 内。
pub(crate) fn follow_scroll(scroll_offset: &mut usize, cursor_line: usize, viewport: usize) {
    if viewport == 0 {
        return;
    }
    if cursor_line < *scroll_offset {
        *scroll_offset = cursor_line;
    } else if cursor_line >= scroll_offset.saturating_add(viewport) {
        *scroll_offset = cursor_line + 1 - viewport;
    }
}

/// 单项过滤判定：filter 为空时全量计入，否则由谓词决定。
/// 空过滤语义与三个 widget 的渲染行为一致（skip 项仅在 env 带过滤时排除）。
pub(crate) fn filter_match<T>(filter: &str, item: &T, pred: fn(&T, &str) -> bool) -> bool {
    filter.is_empty() || pred(item, filter)
}

/// 过滤列表的通用计数。"过滤集合 ↔ 下标 ↔ 渲染"一致性的唯一实现点，
/// 状态层与 widgets 层共用。
pub(crate) fn filtered_len<T>(items: &[T], filter: &str, pred: fn(&T, &str) -> bool) -> usize {
    items
        .iter()
        .filter(|i| filter_match(filter, *i, pred))
        .count()
}

/// 过滤列表的通用下标访问：与 filtered_len 严格同一集合语义。
pub(crate) fn filtered_item_at<'a, T>(
    items: &'a [T],
    filter: &str,
    cursor: usize,
    pred: fn(&T, &str) -> bool,
) -> Option<&'a T> {
    items
        .iter()
        .filter(|i| filter_match(filter, *i, pred))
        .nth(cursor)
}

/// 三个选择列表各自的过滤谓词。谓词按界面各自演化（匹配字段、skip 特例），
/// 因此保留为独立函数作为参数，与共享的计数/下标逻辑分离。
pub(crate) fn product_filter_pred(item: &ProductItem, filter: &str) -> bool {
    super::fuzzy::fuzzy_match(filter, &item.display_name)
        || super::fuzzy::fuzzy_match(filter, &item.id)
}

pub(crate) fn env_filter_pred(item: &EnvItem, filter: &str) -> bool {
    !item.is_skip && super::fuzzy::fuzzy_match(filter, &item.name)
}

pub(crate) fn model_filter_pred(item: &ModelItem, filter: &str) -> bool {
    super::fuzzy::fuzzy_match(filter, &item.id)
        || super::fuzzy::fuzzy_match(filter, &item.display_name)
}

impl ModelManagementState {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.clear_success_message();
    }

    pub fn move_down(&mut self) {
        if self.cursor < self.filtered_count().saturating_sub(1) {
            self.cursor += 1;
        }
        self.clear_success_message();
    }

    /// error 消息固定渲染在列表底部时占用的行数。
    pub fn error_row_count(&self) -> usize {
        usize::from(self.error.is_some())
    }

    /// 调整滚动窗口，使 cursor 始终落在可视区域内。
    pub fn ensure_cursor_visible(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset.saturating_add(viewport) {
            self.scroll_offset = self.cursor.saturating_add(1).saturating_sub(viewport);
        }
    }

    fn clear_success_message(&mut self) {
        if self.error.as_deref().is_some_and(|e| e.starts_with('✓')) {
            self.error = None;
        }
    }

    pub fn selected_model(&self) -> Option<&ModelListItem> {
        self.filtered_models().get(self.cursor).copied()
    }

    pub fn filtered_models(&self) -> Vec<&ModelListItem> {
        if self.filter.is_empty() {
            self.models.iter().collect()
        } else {
            let filter_lower = self.filter.to_lowercase();
            self.models
                .iter()
                .filter(|item| {
                    item.id.to_lowercase().contains(&filter_lower)
                        || item.features.to_lowercase().contains(&filter_lower)
                        || item.thinking.to_lowercase().contains(&filter_lower)
                })
                .collect()
        }
    }

    pub fn filtered_count(&self) -> usize {
        self.filtered_models().len()
    }

    pub fn activate_filter(&mut self) {
        self.filter_active = true;
    }

    pub fn deactivate_filter(&mut self) {
        self.filter_active = false;
        self.filter.clear();
        self.cursor = 0;
        self.scroll_offset = 0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDetailKind {
    Existing { original_id: String },
    Add,
    Copy { source_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDetailField {
    Id,
    ContextWindow,
    MaxOutput,
    Thinking,
    Features,
    ChatBindings,
    ResponsesBindings,
    AnthropicBindings,
    Routes,
}

impl ModelDetailField {
    pub const ALL: [Self; 8] = [
        Self::Id,
        Self::ContextWindow,
        Self::MaxOutput,
        Self::Thinking,
        Self::Features,
        Self::ChatBindings,
        Self::ResponsesBindings,
        Self::AnthropicBindings,
    ];

    pub const ROUTING: [Self; 6] = [
        Self::Id,
        Self::ContextWindow,
        Self::MaxOutput,
        Self::Thinking,
        Self::Features,
        Self::Routes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Id => "model_id",
            Self::ContextWindow => "context_window",
            Self::MaxOutput => "max_output_tokens",
            Self::Thinking => "thinking",
            Self::Features => "features",
            Self::ChatBindings => "openai_chat_providers",
            Self::ResponsesBindings => "openai_responses_providers",
            Self::AnthropicBindings => "anthropic_providers",
            Self::Routes => "reasoning_routes",
        }
    }

    pub fn is_binding(self) -> bool {
        matches!(
            self,
            Self::ChatBindings | Self::ResponsesBindings | Self::AnthropicBindings
        )
    }

    pub fn protocol(self) -> Option<crate::config::Protocol> {
        match self {
            Self::ChatBindings => Some(crate::config::Protocol::OpenaiChatCompletions),
            Self::ResponsesBindings => Some(crate::config::Protocol::OpenaiResponses),
            Self::AnthropicBindings => Some(crate::config::Protocol::Anthropic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelBindingPreview {
    pub name: String,
    pub upstream: String,
    pub cooldown: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRoutePreview {
    pub level: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct ModelDetailState {
    pub kind: ModelDetailKind,
    pub model_id: String,
    pub context_window: String,
    pub max_output_tokens: String,
    pub thinking_enabled: bool,
    pub default_reasoning_level: String,
    pub supported_reasoning_levels: String,
    pub features: String,
    pub chat_bindings: Vec<ModelBindingPreview>,
    pub responses_bindings: Vec<ModelBindingPreview>,
    pub anthropic_bindings: Vec<ModelBindingPreview>,
    pub routes: Vec<ModelRoutePreview>,
    pub is_routing_model: bool,
    pub cursor: usize,
    pub editing: bool,
    pub edit_buffer: String,
    pub edit_cursor: usize,
    pub error: Option<String>,
    /// 内容滚动窗口首行下标（字段行 + 展开的绑定行）
    pub scroll_offset: usize,
}

impl ModelDetailState {
    pub fn is_draft(&self) -> bool {
        !matches!(self.kind, ModelDetailKind::Existing { .. })
    }

    pub fn fields(&self) -> &[ModelDetailField] {
        if self.is_routing_model {
            &ModelDetailField::ROUTING
        } else {
            &ModelDetailField::ALL
        }
    }

    pub fn selected_field(&self) -> ModelDetailField {
        let fields = self.fields();
        fields[self.cursor.min(fields.len().saturating_sub(1))]
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.error = None;
    }

    pub fn move_down(&mut self) {
        let fields = self.fields();
        if self.cursor < fields.len().saturating_sub(1) {
            self.cursor += 1;
        }
        self.error = None;
    }

    fn bindings_len(&self, field: ModelDetailField) -> usize {
        match field {
            ModelDetailField::ChatBindings => self.chat_bindings.len(),
            ModelDetailField::ResponsesBindings => self.responses_bindings.len(),
            ModelDetailField::AnthropicBindings => self.anthropic_bindings.len(),
            ModelDetailField::Routes => self.routes.len(),
            _ => 0,
        }
    }

    /// 当前字段在内容行中的下标（绑定字段会在自身行下展开 bindings.len() 行，routes 会展开 routes.len() 行）。
    pub fn cursor_line_index(&self) -> usize {
        let mut index = self.cursor;
        for (i, field) in self.fields().iter().enumerate() {
            if i >= self.cursor {
                break;
            }
            if field.is_binding() || *field == ModelDetailField::Routes {
                index += self.bindings_len(*field);
            }
        }
        index
    }

    /// error 消息固定渲染在底部时占用的行数（空行 + 消息行）。
    pub fn error_row_count(&self) -> usize {
        if self.error.is_some() { 2 } else { 0 }
    }
}

#[derive(Debug, Clone)]
pub struct ModelDeleteConfirmState {
    pub model_id: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BindingRow {
    pub name: String,
    pub upstream: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct ProtocolBindingEditorState {
    pub model_id: String,
    pub protocol: crate::config::Protocol,
    pub rows: Vec<BindingRow>,
    pub cursor: usize,
    pub adding: bool,
    pub candidates: Vec<String>,
    pub candidate_cursor: usize,
    pub editing_upstream: bool,
    pub upstream_input: String,
    pub upstream_cursor: usize,
    pub remove_confirm: bool,
    pub error: Option<String>,
    /// 内容滚动窗口首行下标（rows 区 + adding 时的候选区）
    pub scroll_offset: usize,
}

impl ProtocolBindingEditorState {
    pub fn selected_row(&self) -> Option<&BindingRow> {
        self.rows.get(self.cursor)
    }

    fn rows_line_count(&self) -> usize {
        if self.rows.is_empty() {
            1 // "(无绑定)" 占位行
        } else {
            self.rows.len()
        }
    }

    /// 当前活动光标所在的内容行下标（adding 时为候选区，否则为 rows 区）。
    pub fn active_cursor_line(&self) -> usize {
        if self.adding {
            self.rows_line_count() + 2 + self.candidate_cursor
        } else if self.rows.is_empty() {
            0
        } else {
            self.cursor
        }
    }

    /// error 消息固定渲染在底部时占用的行数（空行 + 消息行）。
    pub fn error_row_count(&self) -> usize {
        if self.error.is_some() { 2 } else { 0 }
    }
}

pub fn thinking_label(model: &crate::config::ModelConfig) -> String {
    if model.force_no_think == Some(true) || model.supports_thinking == Some(false) {
        return "off".to_string();
    }
    if let Some(level) = &model.default_reasoning_level {
        return level.clone();
    }
    if model.default_think == Some(true)
        || model.force_think == Some(true)
        || model.supports_thinking == Some(true)
        || !model.supported_reasoning_levels.is_empty()
    {
        return "on".to_string();
    }
    "off".to_string()
}

pub fn model_thinking_label(
    cfg: &crate::config::Config,
    id: &str,
    model: &crate::config::ModelConfig,
) -> String {
    if model.is_routing_model() {
        if let Some(level) = cfg.client_default_reasoning_level(id) {
            return level;
        }
        if let Some(levels) = cfg.client_reasoning_levels(id)
            && !levels.is_empty()
        {
            return "on".to_string();
        }
        return "off".to_string();
    }
    thinking_label(model)
}

pub fn load_configured_models(config_path: &std::path::Path) -> Vec<ModelListItem> {
    let Ok(cfg) = crate::config::Config::load(config_path) else {
        return Vec::new();
    };
    let mut models: Vec<ModelListItem> = cfg
        .models
        .iter()
        .map(|(id, model)| {
            let provider_count = crate::config::Protocol::CLIENT_PROTOCOLS
                .into_iter()
                .map(|protocol| model.provider_bindings(protocol).len())
                .sum();
            // a routing model declares no window or features of its own;
            // derive client-visible capabilities from its targets.
            let bounds = cfg.model_capability_bounds(id);
            let (context_window, max_output_tokens) = bounds
                .as_ref()
                .map(|bounds| (bounds.context_window, bounds.max_output_tokens))
                .unwrap_or((model.context_window, model.max_output_tokens));
            let features = bounds
                .as_ref()
                .map(|b| b.features.join(","))
                .unwrap_or_else(|| model.features.join(","));
            let thinking = model_thinking_label(&cfg, id, model);
            ModelListItem {
                id: id.clone(),
                context_window,
                max_output_tokens,
                thinking,
                features,
                provider_count,
                route_count: model.routes().len(),
            }
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

fn binding_previews(
    bindings: &[crate::config::ProviderBinding],
    model_id: &str,
    protocol: crate::config::Protocol,
    cooldown_entries: &[crate::cooldown::CooldownEntry],
    now: u64,
) -> Vec<ModelBindingPreview> {
    bindings
        .iter()
        .map(|binding| {
            let cooldown = crate::cooldown::find_binding_cooldown(
                cooldown_entries,
                model_id,
                &binding.name,
                protocol,
                now,
            )
            .map(|e| crate::cooldown::format_timestamp_local(e.expires_at_unix));
            ModelBindingPreview {
                name: binding.name.clone(),
                upstream: binding.model.clone(),
                cooldown,
            }
        })
        .collect()
}

pub fn build_model_detail(config_path: &std::path::Path, model_id: &str) -> ModelDetailState {
    let cfg = crate::config::Config::load(config_path).ok();
    let model = cfg
        .as_ref()
        .and_then(|cfg| cfg.models.get(model_id))
        .cloned();
    match (cfg.as_ref(), model) {
        (Some(cfg), Some(model)) => {
            let detail_config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
            let detail_dirs = crate::service::StateDirs::resolve(detail_config_dir);
            let cooldown_entries =
                crate::cooldown::read_entries(&detail_dirs.state_dir.join("cooldowns.json"))
                    .unwrap_or_default();
            let now = crate::cooldown::unix_secs();
            let is_routing_model = model.is_routing_model();
            let bounds = cfg.model_capability_bounds(model_id);
            let context_window = bounds
                .as_ref()
                .map(|b| b.context_window)
                .unwrap_or(model.context_window)
                .to_string();
            let max_output_tokens = bounds
                .as_ref()
                .map(|b| b.max_output_tokens)
                .unwrap_or(model.max_output_tokens)
                .to_string();
            let features = bounds
                .as_ref()
                .map(|b| b.features.join(","))
                .unwrap_or_else(|| model.features.join(","));

            let (thinking_enabled, default_reasoning_level, supported_reasoning_levels) =
                if is_routing_model {
                    let client_levels = cfg
                        .client_reasoning_levels(model_id)
                        .map(|entries| {
                            entries
                                .into_iter()
                                .map(|e| e.level().to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let default_level = cfg
                        .client_default_reasoning_level(model_id)
                        .unwrap_or_default();
                    let enabled = !client_levels.is_empty() || !default_level.is_empty();
                    (enabled, default_level, client_levels.join(","))
                } else {
                    let enabled = model.force_no_think != Some(true)
                        && model.supports_thinking != Some(false)
                        && (model.default_think == Some(true)
                            || model.force_think == Some(true)
                            || model.supports_thinking == Some(true)
                            || model.default_reasoning_level.is_some()
                            || !model.supported_reasoning_levels.is_empty());
                    (
                        enabled,
                        model.default_reasoning_level.clone().unwrap_or_default(),
                        model.supported_reasoning_levels.join(","),
                    )
                };

            let routes = model
                .routes()
                .iter()
                .map(|r| ModelRoutePreview {
                    level: r.level.clone(),
                    target: r.target.clone(),
                })
                .collect();

            ModelDetailState {
                kind: ModelDetailKind::Existing {
                    original_id: model_id.to_string(),
                },
                model_id: model_id.to_string(),
                context_window,
                max_output_tokens,
                thinking_enabled,
                default_reasoning_level,
                supported_reasoning_levels,
                features,
                chat_bindings: binding_previews(
                    &model.openai_chat_providers,
                    model_id,
                    crate::config::Protocol::OpenaiChatCompletions,
                    &cooldown_entries,
                    now,
                ),
                responses_bindings: binding_previews(
                    &model.openai_responses_providers,
                    model_id,
                    crate::config::Protocol::OpenaiResponses,
                    &cooldown_entries,
                    now,
                ),
                anthropic_bindings: binding_previews(
                    &model.anthropic_providers,
                    model_id,
                    crate::config::Protocol::Anthropic,
                    &cooldown_entries,
                    now,
                ),
                routes,
                is_routing_model,
                cursor: 0,
                editing: false,
                edit_buffer: String::new(),
                edit_cursor: 0,
                error: None,
                scroll_offset: 0,
            }
        }
        _ => ModelDetailState {
            kind: ModelDetailKind::Add,
            model_id: String::new(),
            context_window: "128000".to_string(),
            max_output_tokens: "8192".to_string(),
            thinking_enabled: false,
            default_reasoning_level: String::new(),
            supported_reasoning_levels: String::new(),
            features: String::new(),
            chat_bindings: Vec::new(),
            responses_bindings: Vec::new(),
            anthropic_bindings: Vec::new(),
            routes: Vec::new(),
            is_routing_model: false,
            cursor: 0,
            editing: true,
            edit_buffer: String::new(),
            edit_cursor: 0,
            error: Some(format!("unknown model {model_id:?}")),
            scroll_offset: 0,
        },
    }
}

pub fn build_add_model_detail() -> ModelDetailState {
    ModelDetailState {
        kind: ModelDetailKind::Add,
        model_id: String::new(),
        context_window: "128000".to_string(),
        max_output_tokens: "8192".to_string(),
        thinking_enabled: false,
        default_reasoning_level: String::new(),
        supported_reasoning_levels: String::new(),
        features: String::new(),
        chat_bindings: Vec::new(),
        responses_bindings: Vec::new(),
        anthropic_bindings: Vec::new(),
        routes: Vec::new(),
        is_routing_model: false,
        cursor: 0,
        editing: true,
        edit_buffer: String::new(),
        edit_cursor: 0,
        error: None,
        scroll_offset: 0,
    }
}

pub fn build_copy_model_detail(
    config_path: &std::path::Path,
    source_id: &str,
) -> Option<ModelDetailState> {
    let cfg = crate::config::Config::load(config_path).ok()?;
    let source_model = cfg.models.get(source_id)?;
    if source_model.is_routing_model() {
        return None;
    }
    let mut state = build_model_detail(config_path, source_id);
    state.kind = ModelDetailKind::Copy {
        source_id: source_id.to_string(),
    };
    state.model_id = generate_default_copy_id(source_id, &cfg.models);
    state.cursor = 0;
    state.editing = true;
    state.edit_buffer = state.model_id.clone();
    state.edit_cursor = state.edit_buffer.len();
    state.error = None;
    Some(state)
}

// ── Product List Builder ─────────────────────────────────────────────────

pub fn build_product_list(_config_path: &std::path::Path) -> Vec<ProductItem> {
    let entries = crate::catalog::built_in_providers();

    let mut items: Vec<ProductItem> = entries
        .iter()
        .map(|entry| {
            let auth_type = match entry.provider.auth_config(entry.id) {
                Ok(crate::config::AuthConfig::OpenaiOauth { .. }) => ProviderAuthType::OpenaiOauth,
                Ok(crate::config::AuthConfig::ApiKeyEnv { .. }) => ProviderAuthType::ApiKey,
                Ok(crate::config::AuthConfig::None) => ProviderAuthType::None,
                Err(_) => ProviderAuthType::ApiKey,
            };

            ProductItem {
                id: entry.id.to_string(),
                display_name: format_display_name(entry.id),
                auth_type,
                endpoint_count: entry.provider.endpoints().len(),
                product_kind: if matches!(
                    entry.provider.auth_config(entry.id),
                    Ok(crate::config::AuthConfig::OpenaiOauth { .. })
                ) {
                    "subscription"
                } else if matches!(
                    entry.provider.auth_config(entry.id),
                    Ok(crate::config::AuthConfig::None)
                ) {
                    "local"
                } else {
                    "payg"
                }
                .to_string(),
                is_custom: false,
            }
        })
        .collect();
    // Sort: non-Ollama first, then Ollama, then custom at end
    items.sort_by(|a, b| {
        if a.id == "ollama" {
            std::cmp::Ordering::Greater
        } else if b.id == "ollama" {
            std::cmp::Ordering::Less
        } else {
            a.display_name.cmp(&b.display_name)
        }
    });
    // Append "Custom provider" item
    items.push(ProductItem {
        id: String::new(),
        display_name: "Custom provider…".to_string(),
        auth_type: ProviderAuthType::None,
        endpoint_count: 0,
        product_kind: "custom".to_string(),
        is_custom: true,
    });
    items
}

fn format_display_name(id: &str) -> String {
    match id {
        "deepseek" => "DeepSeek API".to_string(),
        "openai-payg" => "OpenAI PAYG".to_string(),
        "openai-sub" => "OpenAI Subscription".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        "anthropic" => "Anthropic API".to_string(),
        "ollama" => "Ollama (Local)".to_string(),
        "kimi-platform-global" => "Kimi Open Platform".to_string(),
        "kimi-platform-cn" => "Kimi Open Platform CN".to_string(),
        "kimi-sub" => "Kimi Code".to_string(),
        "zhipu-payg-global" => "Zhipu PAYG Global (Z.ai)".to_string(),
        "zhipu-payg-cn" => "Zhipu PAYG CN".to_string(),
        "zhipu-coding-cn" => "Zhipu Coding Plan CN".to_string(),
        "bailian-coding-plan-cn" => "Bailian Coding Plan CN".to_string(),
        "bailian-payg-cn" => "Bailian PAYG CN".to_string(),
        "bailian-payg-us" => "Bailian PAYG US".to_string(),
        "mimo-payg" => "MiMo PAYG".to_string(),
        "mimo-token-plan-cn" => "MiMo Token Plan CN".to_string(),
        "mimo-token-plan-sgp" => "MiMo Token Plan SGP".to_string(),
        "mimo-token-plan-ams" => "MiMo Token Plan AMS".to_string(),
        "stepfun-payg" => "StepFun PAYG".to_string(),
        "stepfun-step-plan" => "StepFun Step Plan".to_string(),
        _ => id.to_string(),
    }
}

/// Check if an OAuth token exists and is not expired.
/// Returns Some(true) if valid, Some(false) if expired, None if not found.
fn check_oauth_token(
    token_type: &str,
    account_id: &str,
    config_path: &std::path::Path,
) -> Option<bool> {
    let path = crate::auth::state_path_for_config(config_path);
    let (accounts, _skipped) = crate::auth::load_oauth_accounts(&path).ok()?;
    match token_type {
        "openai" => accounts
            .openai
            .get(account_id)
            .map(|entry| !entry.is_expired()),
        _ => None,
    }
}

/// 从配置文件加载已配置的 provider 列表
pub fn load_configured_providers(config_path: &std::path::Path) -> Vec<ProviderListItem> {
    let config = match crate::config::Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(_) => return Vec::new(),
    };

    let providers_config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let providers_dirs = crate::service::StateDirs::resolve(providers_config_dir);
    let cooldown_entries =
        crate::cooldown::read_entries(&providers_dirs.state_dir.join("cooldowns.json"))
            .unwrap_or_default();
    let now = crate::cooldown::unix_secs();

    let mut providers = Vec::new();

    for (name, provider) in &config.providers {
        // 确定产品名（通过匹配 endpoint URL 或标记为 Custom）
        let product = identify_product(provider);

        // 确定认证类型
        let auth_type = match &provider.auth {
            Some(crate::config::AuthConfig::ApiKeyEnv { .. }) => "ApiKey".to_string(),
            Some(crate::config::AuthConfig::OpenaiOauth { .. }) => "OpenAI OAuth".to_string(),
            Some(crate::config::AuthConfig::None) => "None".to_string(),
            None => {
                // 检查是否有 api_key_env
                if provider.api_key_env.is_some() {
                    "ApiKey".to_string()
                } else {
                    "None".to_string()
                }
            }
        };

        // 确定状态（检查实际可用性）
        let status = if provider.api_key_env.is_some() {
            // ApiKey provider: check if env var is actually set
            let env_var = provider.api_key_env.as_deref().unwrap_or("");
            if std::env::var(env_var).is_ok_and(|v| !v.is_empty()) {
                ProviderStatus::Ok
            } else {
                ProviderStatus::Warning // env var not set
            }
        } else {
            match &provider.auth {
                Some(crate::config::AuthConfig::OpenaiOauth { account }) => {
                    let account_id = account.as_deref().unwrap_or(name);
                    match check_oauth_token("openai", account_id, config_path) {
                        Some(true) => ProviderStatus::Ok,
                        Some(false) => ProviderStatus::Warning, // expired
                        None => ProviderStatus::Error,          // not found
                    }
                }
                Some(crate::config::AuthConfig::ApiKeyEnv { env }) => {
                    if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
                        ProviderStatus::Ok
                    } else {
                        ProviderStatus::Warning
                    }
                }
                Some(crate::config::AuthConfig::None) | None => ProviderStatus::Ok,
            }
        };

        // 收集支持的协议
        let mut protocols = Vec::new();
        if provider.openai_chat.is_some() {
            protocols.push("chat".to_string());
        }
        if provider.openai_responses.is_some() {
            protocols.push("responses".to_string());
        }
        if provider.anthropic.is_some() {
            protocols.push("anthropic".to_string());
        }

        let cooldown = crate::cooldown::find_provider_cooldown(&cooldown_entries, name, now)
            .map(|e| crate::cooldown::format_timestamp_local(e.expires_at_unix));

        providers.push(ProviderListItem {
            name: name.clone(),
            product,
            auth_type,
            status,
            protocols,
            cooldown,
        });
    }

    // 按名称排序
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    providers
}

/// Build the provider detail screen used by Enter on the list and by `--select`.
pub fn build_provider_detail(
    config_path: &std::path::Path,
    provider: &ProviderListItem,
    quota_fetched_at: Option<std::time::SystemTime>,
) -> ProviderDetailState {
    let config = crate::config::Config::load(config_path).ok();

    let mut endpoints = Vec::new();
    let mut bound_models = Vec::new();
    let mut compat_info = Vec::new();

    if let Some(ref cfg) = config {
        if let Some(pc) = cfg.providers.get(&provider.name) {
            if let Some(ref ep) = pc.openai_chat {
                endpoints.push(EndpointInfo {
                    protocol: "chat".to_string(),
                    url: ep.url.clone().unwrap_or_default(),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }
            if let Some(ref ep) = pc.openai_responses {
                endpoints.push(EndpointInfo {
                    protocol: "responses".to_string(),
                    url: ep
                        .url
                        .clone()
                        .unwrap_or_else(|| ep.derive_from.clone().unwrap_or_default()),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }
            if let Some(ref ep) = pc.anthropic {
                endpoints.push(EndpointInfo {
                    protocol: "anthropic".to_string(),
                    url: ep
                        .url
                        .clone()
                        .unwrap_or_else(|| ep.derive_from.clone().unwrap_or_default()),
                    kind: if ep.derive_from.is_some() {
                        "derived".to_string()
                    } else {
                        "native".to_string()
                    },
                });
            }

            if let Some(ref ep) = pc.openai_chat
                && let Some(ref compat) = ep.compat
            {
                if let Some(ref tf) = compat.thinking_format {
                    compat_info.push(format!("thinking_format: {}", tf));
                }
                if let Some(sre) = compat.supports_reasoning_effort {
                    compat_info.push(format!("supports_reasoning_effort: {}", sre));
                }
                if let Some(ref mtf) = compat.max_tokens_field {
                    compat_info.push(format!("max_tokens_field: {}", mtf));
                }
            }
        }

        for (model_id, model_config) in &cfg.models {
            let has_binding = model_config
                .openai_chat_providers
                .iter()
                .any(|p| p.name == provider.name)
                || model_config
                    .openai_responses_providers
                    .iter()
                    .any(|p| p.name == provider.name)
                || model_config
                    .anthropic_providers
                    .iter()
                    .any(|p| p.name == provider.name);

            if has_binding {
                bound_models.push(model_id.clone());
            }
        }
    }

    let auth_description = provider.auth_type.clone();
    let auth_status = match provider.status {
        ProviderStatus::Ok => "✓ 已配置".to_string(),
        ProviderStatus::Warning => "⚠ 需要登录".to_string(),
        ProviderStatus::Error => "✗ 错误".to_string(),
    };

    ProviderDetailState {
        name: provider.name.clone(),
        product: provider.product.clone(),
        auth_description,
        auth_status,
        endpoints,
        bound_models,
        compat_info,
        model_cursor: 0,
        scroll_offset: 0,
        quota_fetched_at,
    }
}

/// 识别 provider 属于哪个产品
fn identify_product(provider: &crate::config::ProviderConfig) -> String {
    // 优先使用 product 字段
    if !provider.is_custom_product() {
        return format_display_name(&provider.product);
    }

    // 遍历所有 endpoint URL 识别产品（不限于 openai_chat，纯 Anthropic provider 也能识别）
    for (_, endpoint) in provider.endpoints() {
        let Some(url) = endpoint.url.as_deref() else {
            continue;
        };
        // 匹配已知的产品 URL
        if url.contains("api.deepseek.com") {
            return "DeepSeek API".to_string();
        }
        if url.contains("chatgpt.com") {
            return "OpenAI Subscription".to_string();
        }
        if url.contains("api.openai.com") {
            return "OpenAI PAYG".to_string();
        }
        if url.contains("api.anthropic.com") {
            return "Anthropic API".to_string();
        }
        if url.contains("api.moonshot.ai") {
            return "Kimi Open Platform".to_string();
        }
        if url.contains("api.moonshot.cn") {
            return "Kimi Open Platform CN".to_string();
        }
        if url.contains("api.kimi.com/coding") {
            return "Kimi Code".to_string();
        }
        if url.contains("api.xiaomimimo.com") {
            return "MiMo PAYG".to_string();
        }
        if url.contains("token-plan-cn.xiaomimimo.com") {
            return "MiMo Token Plan CN".to_string();
        }
        if url.contains("token-plan-sgp.xiaomimimo.com") {
            return "MiMo Token Plan SGP".to_string();
        }
        if url.contains("token-plan-ams.xiaomimimo.com") {
            return "MiMo Token Plan AMS".to_string();
        }
        if url.contains("open.bigmodel.cn") && url.contains("/coding/") {
            return "Zhipu Coding Plan CN".to_string();
        }
        if url.contains("open.bigmodel.cn") {
            return "Zhipu PAYG CN".to_string();
        }
        if url.contains("coding.dashscope.aliyuncs.com") {
            return "Bailian Coding Plan CN".to_string();
        }
        if url.contains("dashscope.aliyuncs.com") {
            return "Bailian PAYG CN".to_string();
        }
        if url.contains("dashscope-us.aliyuncs.com") {
            return "Bailian PAYG US".to_string();
        }
        if url.contains("api.stepfun.ai/step_plan") {
            return "StepFun Step Plan".to_string();
        }
        if url.contains("api.stepfun.ai") {
            return "StepFun PAYG".to_string();
        }
        if url.contains("api.z.ai") {
            return "Z.ai PAYG Global".to_string();
        }
        if url.contains("openrouter.ai") {
            return "OpenRouter".to_string();
        }
        if url.contains("127.0.0.1:11434") || url.contains("localhost:11434") {
            return "Ollama (Local)".to_string();
        }
    }

    // 无法识别，标记为 Custom
    "Custom".to_string()
}

// ── Provider Naming Helpers ──────────────────────────────────────────────

/// Compute the next available instance ID for a product.
///
/// Scans existing provider names in `providers` and finds the smallest `N ≥ 2`
/// such that `{product}-N` is not already taken.
///
/// Only names matching the exact pattern `{product}-N` (where N is a positive
/// integer ≥ 2) participate in counting. Custom names like `my-deepseek` are
/// ignored for counting purposes (but still checked for conflicts separately).
///
/// # Examples
///
/// ```ignore
/// // existing: "deepseek" → returns "deepseek-2"
/// // existing: "deepseek", "deepseek-2" → returns "deepseek-3"
/// // existing: "deepseek", "my-deepseek" → returns "deepseek-2" (my-deepseek ignored)
/// // existing: "deepseek", "deepseek-2", "deepseek-4" → returns "deepseek-3" (fill gap)
/// ```
pub fn next_instance_id(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
    product: &str,
) -> String {
    // Collect all N from existing "{product}-N" names
    let mut used_suffixes = std::collections::BTreeSet::new();
    let prefix = format!("{}-", product);
    for name in providers.keys() {
        if let Some(suffix) = name.strip_prefix(&prefix)
            && let Ok(n) = suffix.parse::<usize>()
            && n >= 2
        {
            used_suffixes.insert(n);
        }
    }

    // Find the smallest N ≥ 2 not in used_suffixes
    let mut n = 2;
    loop {
        if !used_suffixes.contains(&n) {
            return format!("{}-{}", product, n);
        }
        n += 1;
    }
}

/// Check whether a provider name is already taken.
pub fn is_provider_name_taken(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
    name: &str,
) -> bool {
    providers.contains_key(name)
}

/// Build CopyConfigure state from a selected provider.
pub fn build_copy_configure_state(
    config_path: &std::path::Path,
    source_id: &str,
    source_product_display: &str,
    source_auth_label: &str,
) -> Option<CopyConfigureState> {
    let config = crate::config::Config::load(config_path).ok()?;
    let provider = config.providers.get(source_id)?;
    let product_key = if provider.is_custom_product() {
        source_id.to_string()
    } else {
        provider.product.clone()
    };
    let default_name = next_instance_id(&config.providers, &product_key);
    let validation = if is_provider_name_taken(&config.providers, &default_name) {
        NameValidation::Conflict
    } else {
        NameValidation::Available
    };
    let auth = provider.auth_config(source_id).ok()?;
    let (auth_kind, source_api_key_env) = match auth {
        crate::config::AuthConfig::ApiKeyEnv { env } => (CopyAuthKind::ApiKey, Some(env)),
        crate::config::AuthConfig::OpenaiOauth { .. } => (CopyAuthKind::OpenaiOauth, None),
        crate::config::AuthConfig::None => (CopyAuthKind::None, None),
    };
    let mut endpoints = Vec::new();
    if provider.openai_chat.is_some() {
        endpoints.push("chat");
    }
    if provider.openai_responses.is_some() {
        endpoints.push("responses");
    }
    if provider.anthropic.is_some() {
        endpoints.push("anthropic");
    }
    Some(CopyConfigureState {
        source_id: source_id.to_string(),
        source_product: source_product_display.to_string(),
        source_auth_label: source_auth_label.to_string(),
        source_endpoints: endpoints.join(" · "),
        auth_kind,
        default_name: default_name.clone(),
        input: default_name,
        editing: false,
        cursor_pos: 0,
        validation,
        step: CopyConfigureStep::Name,
        source_api_key_env: source_api_key_env.clone(),
        env_input: source_api_key_env.unwrap_or_default(),
        env_editing: false,
        env_cursor_pos: 0,
        error: None,
    })
}

/// Check whether a product already has at least one provider in the config.
/// A product "exists" if any provider name equals the product ID or matches
/// the `{product}-N` pattern.
pub fn product_already_configured(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
    product_id: &str,
) -> bool {
    // Direct match: provider name == product_id
    if providers.contains_key(product_id) {
        return true;
    }
    // Suffix match: any provider name matches "{product_id}-N"
    let prefix = format!("{}-", product_id);
    for name in providers.keys() {
        if let Some(suffix) = name.strip_prefix(&prefix)
            && suffix.parse::<usize>().is_ok()
        {
            return true;
        }
    }
    false
}

/// Build a `ProviderNamingState` for a product that already has a provider.
pub fn build_naming_state(
    config_path: &std::path::Path,
    product_item: ProductItem,
) -> ProviderNamingState {
    let providers = crate::config::Config::load(config_path)
        .map(|cfg| cfg.providers)
        .unwrap_or_default();

    let default_name = next_instance_id(&providers, &product_item.id);
    let validation = if is_provider_name_taken(&providers, &default_name) {
        NameValidation::Conflict
    } else {
        NameValidation::Available
    };

    ProviderNamingState {
        product_id: product_item.id.clone(),
        product_display_name: product_item.display_name.clone(),
        default_name: default_name.clone(),
        input: default_name,
        editing: false,
        cursor_pos: 0,
        validation,
        product_item,
    }
}

// ── Env Var Usage Detection ─────────────────────────────────────────────

/// Build a map of env var name → provider name for all configured providers.
/// Used to detect when the user selects an env var already in use.
pub fn build_env_usage_map(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for (name, provider) in providers {
        // Check api_key_env field
        if let Some(ref env) = provider.api_key_env {
            map.insert(env.clone(), name.clone());
        }
        // Check auth field for ApiKeyEnv variant
        if let Some(crate::config::AuthConfig::ApiKeyEnv { env }) = &provider.auth {
            map.insert(env.clone(), name.clone());
        }
    }
    map
}

/// Find which provider (if any) is using a given env var.
/// Returns the provider name, or None if the env var is not in use.
pub fn find_env_user(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
    env_var: &str,
) -> Option<String> {
    let map = build_env_usage_map(providers);
    map.get(env_var).cloned()
}

/// Get all provider names that share the same product.
pub fn find_providers_by_product(
    providers: &std::collections::BTreeMap<String, crate::config::ProviderConfig>,
    product_id: &str,
) -> Vec<String> {
    providers
        .iter()
        .filter(|(_, p)| p.product == product_id)
        .map(|(name, _)| name.clone())
        .collect()
}

/// Build the set of model IDs that already have provider bindings for the given product.
///
/// Used to populate `ModelSelectionState.configured` so the TUI can mark
/// already-bound models with a ★ indicator.
pub fn build_configured_set(
    config_path: &std::path::Path,
    product_id: &str,
    items: &[ModelItem],
) -> HashSet<String> {
    let config = match crate::config::Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(_) => return HashSet::new(),
    };

    // Collect provider names that belong to this product
    let product_providers: std::collections::HashSet<String> =
        find_providers_by_product(&config.providers, product_id)
            .into_iter()
            .collect();

    if product_providers.is_empty() {
        return HashSet::new();
    }

    let mut configured = HashSet::new();
    for item in items {
        if let Some(model_config) = config.models.get(&item.id) {
            for protocol in crate::config::Protocol::CLIENT_PROTOCOLS {
                let bindings = model_config.provider_bindings(protocol);
                if bindings.iter().any(|b| product_providers.contains(&b.name)) {
                    configured.insert(item.id.clone());
                    break; // no need to check more protocols for this model
                }
            }
        }
    }
    configured
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EndpointConfig, ProviderConfig};

    fn endpoint(url: &str) -> EndpointConfig {
        EndpointConfig {
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn provider_tui_select_opens_detail_for_known_provider() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();

        let app = AppModel::new(
            path.clone(),
            EntryMode::ProviderTui {
                select: Some("deepseek".into()),
            },
        );
        match app.screen {
            Screen::ProviderDetail(state) => {
                assert_eq!(state.name, "deepseek");
                assert!(!state.endpoints.is_empty());
            }
            other => panic!("expected provider detail, got {other:?}"),
        }
    }

    #[test]
    fn provider_tui_select_unknown_stays_on_list_with_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        crate::config::init_config(&path).unwrap();

        let app = AppModel::new(
            path,
            EntryMode::ProviderTui {
                select: Some("missing".into()),
            },
        );
        match app.screen {
            Screen::ProviderManagement(state) => {
                assert!(
                    state
                        .error
                        .as_deref()
                        .is_some_and(|err| err.contains("unknown provider"))
                );
            }
            other => panic!("expected provider list, got {other:?}"),
        }
    }

    #[test]
    fn model_list_scroll_follows_cursor_down_and_up() {
        let mut state = ModelManagementState {
            models: Vec::new(),
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
        };
        // viewport=5：cursor 移出窗口下沿时 offset 跟随
        state.cursor = 5;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 1);
        // 窗口内上移不动 offset
        state.cursor = 4;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 1);
        // 移出窗口上沿时 offset 跟随上移
        state.cursor = 0;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 0);
        // viewport 为 0 时不动作
        state.scroll_offset = 3;
        state.ensure_cursor_visible(0);
        assert_eq!(state.scroll_offset, 3);
    }

    #[test]
    fn provider_list_scroll_follows_cursor_down_and_up() {
        let mut state = ProviderManagementState {
            providers: Vec::new(),
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            scroll_offset: 0,
            quota_fetched_at: None,
        };
        state.cursor = 5;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 1);
        state.cursor = 4;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 1);
        state.cursor = 0;
        state.ensure_cursor_visible(5);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn env_filter_excludes_skip_item_and_follows_render_predicate() {
        let mut state = EnvVarSelectionState {
            items: vec![
                EnvItem {
                    name: "SKIP_ME".to_string(),
                    is_skip: true,
                    recommended: false,
                },
                EnvItem {
                    name: "MY_API_KEY".to_string(),
                    is_skip: false,
                    recommended: false,
                },
            ],
            cursor: 0,
            filter: String::new(),
            filter_active: false,
            error: None,
            product_name: "test".to_string(),
            env_in_use_warning: None,
        };
        // 无过滤：skip 项计入（与 env_list 一致）
        assert!(state.filtered_item().is_some());

        // 过滤激活：skip 项永不匹配；fuzzy 子串匹配正常项
        state.filter = "api".to_string();
        state.cursor = 0;
        let item = state.filtered_item().expect("filtered item");
        assert_eq!(item.name, "MY_API_KEY");

        // 过滤无匹配时返回 None（调用方不得回退到未过滤下标）
        state.filter = "zzz_no_match".to_string();
        assert!(state.filtered_item().is_none());
    }

    #[test]
    fn provider_management_help_is_split_and_keeps_core_actions_visible() {
        let app = AppModel::new(
            PathBuf::from("/tmp/llm-proxy-test-config.toml"),
            EntryMode::ProviderTui { select: None },
        );
        let help = app.help_text();
        let lines: Vec<_> = help.lines().collect();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("[m] Models"));
        assert!(lines[0].contains("[e] Edit"));
        assert!(lines[0].contains("[Enter] Detail"));
        assert!(lines[0].contains("[q] Quit"));
        assert!(lines.iter().all(|line| line.len() <= 80));
    }

    #[test]
    fn identify_product_matches_anthropic_only_provider() {
        // 纯 Anthropic provider（无 openai_chat）不应再被标为 Custom
        let provider = ProviderConfig {
            anthropic: Some(endpoint("https://api.anthropic.com/v1/messages")),
            ..Default::default()
        };
        assert_eq!(identify_product(&provider), "Anthropic API");
    }

    #[test]
    fn identify_product_returns_custom_for_unknown_url() {
        let provider = ProviderConfig {
            anthropic: Some(endpoint("https://example.com/v1/messages")),
            ..Default::default()
        };
        assert_eq!(identify_product(&provider), "Custom");
    }

    #[test]
    fn identify_product_prefers_openai_chat_url() {
        // 多 endpoint 时 openai_chat 优先（endpoints() 顺序保证）
        let provider = ProviderConfig {
            openai_chat: Some(endpoint("https://api.deepseek.com/v1/chat/completions")),
            anthropic: Some(endpoint("https://api.anthropic.com/v1/messages")),
            ..Default::default()
        };
        assert_eq!(identify_product(&provider), "DeepSeek API");
    }

    // ── next_instance_id tests ──────────────────────────────────────────

    fn empty_providers() -> std::collections::BTreeMap<String, ProviderConfig> {
        std::collections::BTreeMap::new()
    }

    fn providers_with(names: &[&str]) -> std::collections::BTreeMap<String, ProviderConfig> {
        let mut map = std::collections::BTreeMap::new();
        for name in names {
            map.insert(name.to_string(), ProviderConfig::default());
        }
        map
    }

    #[test]
    fn next_instance_id_empty_returns_2() {
        let providers = empty_providers();
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    #[test]
    fn next_instance_id_only_base_name() {
        // Only "deepseek" exists → next is "deepseek-2"
        let providers = providers_with(&["deepseek"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    #[test]
    fn next_instance_id_base_and_2() {
        // "deepseek" + "deepseek-2" → "deepseek-3"
        let providers = providers_with(&["deepseek", "deepseek-2"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-3");
    }

    #[test]
    fn next_instance_id_fills_gap() {
        // "deepseek" + "deepseek-2" + "deepseek-4" → "deepseek-3" (fill gap)
        let providers = providers_with(&["deepseek", "deepseek-2", "deepseek-4"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-3");
    }

    #[test]
    fn next_instance_id_ignores_custom_names() {
        // "deepseek" + "my-deepseek" → "deepseek-2" (my-deepseek doesn't count)
        let providers = providers_with(&["deepseek", "my-deepseek"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    #[test]
    fn next_instance_id_ignores_non_numeric_suffix() {
        // "deepseek" + "deepseek-pro" → "deepseek-2" (non-numeric suffix ignored)
        let providers = providers_with(&["deepseek", "deepseek-pro"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    #[test]
    fn next_instance_id_ignores_suffix_1() {
        // "deepseek-1" is NOT a valid suffix (N must be ≥ 2), so next is "deepseek-2"
        let providers = providers_with(&["deepseek", "deepseek-1"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    #[test]
    fn next_instance_id_consecutive_sequence() {
        // "deepseek" + "deepseek-2" + "deepseek-3" + "deepseek-4" → "deepseek-5"
        let providers = providers_with(&["deepseek", "deepseek-2", "deepseek-3", "deepseek-4"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-5");
    }

    #[test]
    fn next_instance_id_different_product() {
        // "deepseek" + "deepseek-2" exist, but we ask for "kimi-code" → "kimi-code-2"
        let providers = providers_with(&["deepseek", "deepseek-2"]);
        assert_eq!(next_instance_id(&providers, "kimi-code"), "kimi-code-2");
    }

    #[test]
    fn next_instance_id_similar_product_prefix() {
        // "deep" + "deep-2" exist; asking for "deepseek" should NOT be affected
        let providers = providers_with(&["deep", "deep-2"]);
        assert_eq!(next_instance_id(&providers, "deepseek"), "deepseek-2");
    }

    // ── is_provider_name_taken tests ────────────────────────────────────

    #[test]
    fn name_taken_when_exists() {
        let providers = providers_with(&["deepseek", "deepseek-2"]);
        assert!(is_provider_name_taken(&providers, "deepseek"));
        assert!(is_provider_name_taken(&providers, "deepseek-2"));
    }

    #[test]
    fn name_not_taken_when_absent() {
        let providers = providers_with(&["deepseek"]);
        assert!(!is_provider_name_taken(&providers, "deepseek-2"));
        assert!(!is_provider_name_taken(&providers, "my-deepseek"));
    }

    // ── product_already_configured tests ────────────────────────────────

    #[test]
    fn product_configured_direct_match() {
        let providers = providers_with(&["deepseek"]);
        assert!(product_already_configured(&providers, "deepseek"));
    }

    #[test]
    fn product_configured_suffix_match() {
        let providers = providers_with(&["deepseek-2"]);
        assert!(product_already_configured(&providers, "deepseek"));
    }

    #[test]
    fn product_not_configured() {
        let providers = providers_with(&["kimi-code"]);
        assert!(!product_already_configured(&providers, "deepseek"));
    }

    #[test]
    fn product_not_configured_empty() {
        let providers = empty_providers();
        assert!(!product_already_configured(&providers, "deepseek"));
    }

    #[test]
    fn product_not_configured_custom_name_only() {
        // "my-deepseek" is a custom name, not matching the product pattern
        let providers = providers_with(&["my-deepseek"]);
        assert!(!product_already_configured(&providers, "deepseek"));
    }

    // ── build_env_usage_map / find_env_user tests ───────────────────────

    fn providers_with_env(
        pairs: &[(&str, Option<&str>)],
    ) -> std::collections::BTreeMap<String, ProviderConfig> {
        let mut map = std::collections::BTreeMap::new();
        for (name, env) in pairs {
            let mut cfg = ProviderConfig::default();
            if let Some(e) = env {
                cfg.api_key_env = Some(e.to_string());
            }
            map.insert(name.to_string(), cfg);
        }
        map
    }

    #[test]
    fn env_usage_map_empty_when_no_env() {
        let providers = providers_with(&["deepseek", "kimi"]);
        let map = build_env_usage_map(&providers);
        assert!(map.is_empty());
    }

    #[test]
    fn env_usage_map_detects_api_key_env() {
        let providers = providers_with_env(&[
            ("deepseek", Some("DEEPSEEK_API_KEY")),
            ("kimi", Some("KIMI_API_KEY")),
        ]);
        let map = build_env_usage_map(&providers);
        assert_eq!(map.get("DEEPSEEK_API_KEY"), Some(&"deepseek".to_string()));
        assert_eq!(map.get("KIMI_API_KEY"), Some(&"kimi".to_string()));
    }

    #[test]
    fn find_env_user_returns_provider_name() {
        let providers = providers_with_env(&[("deepseek", Some("DEEPSEEK_API_KEY"))]);
        assert_eq!(
            find_env_user(&providers, "DEEPSEEK_API_KEY"),
            Some("deepseek".to_string())
        );
    }

    #[test]
    fn find_env_user_returns_none_for_unused() {
        let providers = providers_with_env(&[("deepseek", Some("DEEPSEEK_API_KEY"))]);
        assert_eq!(find_env_user(&providers, "OTHER_KEY"), None);
    }

    #[test]
    fn find_env_user_detects_auth_api_key_env() {
        let mut map = std::collections::BTreeMap::new();
        let cfg = ProviderConfig {
            auth: Some(crate::config::AuthConfig::ApiKeyEnv {
                env: "MY_KEY".to_string(),
            }),
            ..Default::default()
        };
        map.insert("my-provider".to_string(), cfg);
        assert_eq!(
            find_env_user(&map, "MY_KEY"),
            Some("my-provider".to_string())
        );
    }

    // ── find_providers_by_product tests ─────────────────────────────────

    #[test]
    fn find_providers_by_product_returns_matching() {
        let mut providers = std::collections::BTreeMap::new();
        let ds1 = ProviderConfig {
            product: "deepseek".to_string(),
            ..Default::default()
        };
        let ds2 = ProviderConfig {
            product: "deepseek".to_string(),
            ..Default::default()
        };
        let kimi = ProviderConfig {
            product: "kimi".to_string(),
            ..Default::default()
        };
        providers.insert("deepseek".to_string(), ds1);
        providers.insert("deepseek-2".to_string(), ds2);
        providers.insert("kimi".to_string(), kimi);

        let result = find_providers_by_product(&providers, "deepseek");
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"deepseek".to_string()));
        assert!(result.contains(&"deepseek-2".to_string()));
    }

    #[test]
    fn find_providers_by_product_empty_when_no_match() {
        let providers = providers_with(&["deepseek"]);
        let result = find_providers_by_product(&providers, "kimi");
        assert!(result.is_empty());
    }

    // ── FallbackConfigState tests ───────────────────────────────────────

    #[test]
    fn fallback_state_toggle_option() {
        let mut state = FallbackConfigState {
            target_providers: vec!["deepseek".to_string()],
            target_cursor: 0,
            options: vec![
                FallbackOption {
                    model_id: "model-a".to_string(),
                    model_display_name: "Model A".to_string(),
                    endpoint: "chat".to_string(),
                    selected: false,
                },
                FallbackOption {
                    model_id: "model-b".to_string(),
                    model_display_name: "Model B".to_string(),
                    endpoint: "chat".to_string(),
                    selected: false,
                },
            ],
            option_cursor: 0,
            focus: FallbackFocus::Options,
            error: None,
            skipped: false,
            source: FallbackSource::ConnectFlow,
            scroll_offset: 0,
        };

        // Toggle first option
        state.toggle_option();
        assert!(state.options[0].selected);
        assert!(!state.options[1].selected);

        // Move to second and toggle
        state.move_down();
        state.toggle_option();
        assert!(state.options[0].selected);
        assert!(state.options[1].selected);

        // Toggle first off again
        state.move_up();
        state.toggle_option();
        assert!(!state.options[0].selected);
        assert!(state.options[1].selected);
    }

    #[test]
    fn fallback_state_selected_options() {
        let state = FallbackConfigState {
            target_providers: vec![],
            target_cursor: 0,
            options: vec![
                FallbackOption {
                    model_id: "a".to_string(),
                    model_display_name: "A".to_string(),
                    endpoint: "chat".to_string(),
                    selected: true,
                },
                FallbackOption {
                    model_id: "b".to_string(),
                    model_display_name: "B".to_string(),
                    endpoint: "chat".to_string(),
                    selected: false,
                },
                FallbackOption {
                    model_id: "c".to_string(),
                    model_display_name: "C".to_string(),
                    endpoint: "chat".to_string(),
                    selected: true,
                },
            ],
            option_cursor: 0,
            focus: FallbackFocus::Options,
            error: None,
            skipped: false,
            source: FallbackSource::ConnectFlow,
            scroll_offset: 0,
        };

        let selected = state.selected_options();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].model_id, "a");
        assert_eq!(selected[1].model_id, "c");
    }

    #[test]
    fn fallback_state_navigation_respects_focus() {
        let mut state = FallbackConfigState {
            target_providers: vec!["p1".to_string(), "p2".to_string(), "p3".to_string()],
            target_cursor: 0,
            options: vec![
                FallbackOption {
                    model_id: "a".to_string(),
                    model_display_name: "A".to_string(),
                    endpoint: "chat".to_string(),
                    selected: false,
                },
                FallbackOption {
                    model_id: "b".to_string(),
                    model_display_name: "B".to_string(),
                    endpoint: "chat".to_string(),
                    selected: false,
                },
            ],
            option_cursor: 0,
            focus: FallbackFocus::TargetProvider,
            error: None,
            skipped: false,
            source: FallbackSource::ConnectFlow,
            scroll_offset: 0,
        };

        // Move down in target focus → target_cursor changes, option_cursor doesn't
        state.move_down();
        assert_eq!(state.target_cursor, 1);
        assert_eq!(state.option_cursor, 0);

        // Switch to options focus
        state.focus = FallbackFocus::Options;
        state.move_down();
        assert_eq!(state.target_cursor, 1); // unchanged
        assert_eq!(state.option_cursor, 1); // changed

        // saturating_sub on move_up
        state.move_up();
        state.move_up(); // should not underflow
        assert_eq!(state.option_cursor, 0);
    }

    #[test]
    fn fallback_state_selected_target() {
        let state = FallbackConfigState {
            target_providers: vec!["deepseek".to_string(), "deepseek-2".to_string()],
            target_cursor: 1,
            options: vec![],
            option_cursor: 0,
            focus: FallbackFocus::TargetProvider,
            error: None,
            skipped: false,
            source: FallbackSource::ConnectFlow,
            scroll_offset: 0,
        };
        assert_eq!(state.selected_target(), Some("deepseek-2"));
    }

    #[test]
    fn routing_model_derives_capabilities_in_list_and_detail() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[server]
listen = "127.0.0.1:18990"

[providers.mock]
api_key_env = "MOCK_API_KEY"

[providers.mock.openai_chat]
url = "http://127.0.0.1:18991/chat/completions"

[models.target-low]
context_window = 1000
max_output_tokens = 100
features = ["image_input", "tools"]
default_reasoning_level = "low"

[[models.target-low.reasoning_levels]]
level = "low"
api_value = "1000"

[[models.target-low.openai_chat_providers]]
name = "mock"
model = "upstream-low"

[models.target-high]
context_window = 1000
max_output_tokens = 100
features = ["image_input", "tools"]
default_reasoning_level = "high"

[[models.target-high.reasoning_levels]]
level = "high"
api_value = "-1"

[[models.target-high.openai_chat_providers]]
name = "mock"
model = "upstream-high"

[models.router-lp]
reasoning_routes = [
    { level = "low", target = "target-low" },
    { level = "high", target = "target-high" },
]
"#,
        )
        .expect("write config");

        // 1. Check load_configured_models
        let models = load_configured_models(&config_path);
        let router = models
            .iter()
            .find(|m| m.id == "router-lp")
            .expect("router-lp in list");
        assert_eq!(router.context_window, 1000);
        assert_eq!(router.max_output_tokens, 100);
        assert_eq!(router.thinking, "low");
        assert_eq!(router.features, "image_input,tools");
        assert_eq!(router.provider_count, 0);
        assert_eq!(router.route_count, 2);

        // 2. Check build_model_detail
        let detail = build_model_detail(&config_path, "router-lp");
        assert!(detail.is_routing_model);
        assert_eq!(detail.context_window, "1000");
        assert_eq!(detail.max_output_tokens, "100");
        assert!(detail.thinking_enabled);
        assert_eq!(detail.default_reasoning_level, "low");
        assert_eq!(detail.supported_reasoning_levels, "low,high");
        assert_eq!(detail.features, "image_input,tools");
        assert_eq!(detail.routes.len(), 2);
        assert_eq!(detail.routes[0].level, "low");
        assert_eq!(detail.routes[0].target, "target-low");
        assert_eq!(detail.routes[1].level, "high");
        assert_eq!(detail.routes[1].target, "target-high");
        assert_eq!(detail.fields(), &ModelDetailField::ROUTING);
        assert_eq!(detail.bindings_len(ModelDetailField::Routes), 2);

        // 3. Check copy is rejected for routing model
        assert!(build_copy_model_detail(&config_path, "router-lp").is_none());
    }
}
