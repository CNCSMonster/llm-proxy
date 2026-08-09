#![allow(dead_code)] // TUI model fields reserved for future screens

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

/// Entry mode for the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryMode {
    /// `connect` / `provider add` — product selection wizard
    Connect,
    /// `provider` — full provider management TUI (future)
    ProviderTui,
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
}

impl AppModel {
    pub fn new(config_path: PathBuf, entry_mode: EntryMode) -> Self {
        let products = build_product_list();
        let screen = match &entry_mode {
            EntryMode::ProviderTui => {
                // 加载已配置的 provider 列表
                let providers = load_configured_providers(&config_path);
                Screen::ProviderManagement(ProviderManagementState {
                    providers,
                    cursor: 0,
                    filter: String::new(),
                    filter_active: false,
                    error: None,
                })
            }
            EntryMode::OAuthLogin(provider_id) => {
                // Find the product's auth type and start at the right screen
                let product = products.iter().find(|p| p.id == *provider_id);
                match product.map(|p| &p.auth_type) {
                    Some(ProviderAuthType::OpenaiOauth) => {
                        Screen::OAuthDeviceCode(OAuthDeviceCodeState {
                            provider_name: provider_id.clone(),
                            device_code: String::new(),
                            verification_url: String::new(),
                            expires_at: std::time::Instant::now()
                                + std::time::Duration::from_secs(15 * 60),
                            copied: false,
                            polling: false,
                            retry_count: 0,
                            last_error: None,
                            error: None,
                        })
                    }
                    Some(ProviderAuthType::AntigravityOauth) => {
                        Screen::AntigravityLogin(AntigravityLoginState::new(provider_id.clone()))
                    }
                    _ => Screen::ProductSelection(ProductSelectionState {
                        items: products,
                        cursor: 0,
                        filter: String::new(),
                        filter_active: false,
                        error: None,
                    }),
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
            Screen::AntigravityLogin(s) => s.error.as_deref(),
            Screen::ResetUsageConfirm(s) => s.error.as_deref(),
            Screen::FallbackConfig(s) => s.error.as_deref(),
            Screen::CopyModelConfirm(s) => s.error.as_deref(),
            Screen::ModelRename(s) => s.error.as_deref(),
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
                    "[/] Search  [a] Add  [d] Delete  [f] Fallback  [l] Login  [u] Usage  [R] Reset Usage  [r] Refresh  [Enter] Detail  [q] Quit"
                }
            }
            Screen::ProviderDetail(s) => {
                if s.bound_models.is_empty() {
                    "[e] Edit  [d] Delete  [Esc] Back"
                } else {
                    "[↑/↓] Select Model  [r] Rename  [e] Edit  [d] Delete  [Esc] Back"
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
            Screen::Verifying(_) => "Verifying... please wait",
            Screen::WarningConfirm(_) => "[↑/↓/j/k] Move  [Enter] Confirm",
            Screen::Done(_) => "[Enter] Add Another  [q] Quit",
            Screen::OAuthName(_) => "[↑/↓/j/k] Move  [Enter] Confirm  [Esc] Back",
            Screen::OAuthDeviceCode(_) => "[c] Copy  [o] Open Browser  [s] Stop  [Esc] Skip",
            Screen::AntigravityLogin(_) => {
                "[c] Copy URL  [o] Open Browser  [Enter] Submit  [Esc] Back"
            }
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
    CustomProviderEditor(CustomProviderEditorState),
    ProviderNaming(ProviderNamingState),
    OAuthName(OAuthNameState),
    OAuthDeviceCode(OAuthDeviceCodeState),
    AntigravityLogin(AntigravityLoginState),
    WarningConfirm(WarningConfirmState),
    Verifying(VerifyingState),
    OAuthOverwrite(OAuthOverwriteState),
    Done(DoneState),
    /// Fallback 配置界面（重复 connect 时，env 选择后可进入）
    FallbackConfig(FallbackConfigState),
    /// Model 复制确认界面
    CopyModelConfirm(CopyModelConfirmState),
    /// Model 重命名界面
    ModelRename(ModelRenameState),
    Quit,
}

// ── State Structs ────────────────────────────────────────────────────────

/// Provider 认证类型（互斥枚举）
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAuthType {
    ApiKey,
    OpenaiOauth,
    AntigravityOauth,
    None,
}

impl ProviderAuthType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ApiKey => "API Key",
            Self::OpenaiOauth => "OAuth",
            Self::AntigravityOauth => "OAuth",
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
    pub auth_status: Option<String>, // "✓ Ready", "⚠ Not logged in", "⚠ Expired", etc.
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
}

/// Provider 状态
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderStatus {
    Ok,
    Warning,
    Error,
}

/// Provider 管理面板状态
#[derive(Debug, Clone)]
pub struct ProviderManagementState {
    pub providers: Vec<ProviderListItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
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
    fn filtered_count(&self) -> usize {
        if self.filter.is_empty() {
            self.items.len()
        } else {
            self.items
                .iter()
                .filter(|i| {
                    i.display_name
                        .to_lowercase()
                        .contains(&self.filter.to_lowercase())
                })
                .count()
        }
    }
    pub fn filtered_item(&self) -> Option<&ProductItem> {
        let mut idx = 0usize;
        for item in &self.items {
            if self.filter.is_empty()
                || item
                    .display_name
                    .to_lowercase()
                    .contains(&self.filter.to_lowercase())
            {
                if idx == self.cursor {
                    return Some(item);
                }
                idx += 1;
            }
        }
        None
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
    fn filtered_count(&self) -> usize {
        if self.filter.is_empty() {
            self.items.len()
        } else {
            self.items
                .iter()
                .filter(|i| i.name.to_lowercase().contains(&self.filter.to_lowercase()))
                .count()
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelItem {
    pub id: String,
    pub display_name: String,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub supports_image: bool,
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
    fn filtered_count(&self) -> usize {
        if self.filter.is_empty() {
            self.items.len()
        } else {
            self.items
                .iter()
                .filter(|i| {
                    i.display_name
                        .to_lowercase()
                        .contains(&self.filter.to_lowercase())
                })
                .count()
        }
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

#[derive(Debug, Clone)]
pub struct AntigravityLoginState {
    pub provider_name: String,
    pub auth_url: String,
    /// PKCE code_verifier：进入屏幕时生成，用户提交 code 后用于 OAuth exchange。
    pub verifier: String,
    pub copied: bool,
    pub input: String,
    pub input_active: bool,
    pub error: Option<String>,
    /// 用户按 Enter 提交 code 后置 true，主循环据此用保存的 verifier 执行非交互式 exchange。
    pub submitted: bool,
}

impl AntigravityLoginState {
    /// 进入登录屏时生成 PKCE verifier + 授权 URL（自包含登录，不依赖 CLI 交互）。
    pub fn new(provider_name: String) -> Self {
        let (auth_url, verifier) = match crate::auth::generate_antigravity_auth_url() {
            Ok(pair) => pair,
            Err(err) => {
                return Self {
                    provider_name,
                    auth_url: String::new(),
                    verifier: String::new(),
                    copied: false,
                    input: String::new(),
                    input_active: false,
                    error: Some(format!("Failed to generate auth URL: {err}")),
                    submitted: false,
                };
            }
        };
        Self {
            provider_name,
            auth_url,
            verifier,
            copied: false,
            input: String::new(),
            input_active: false,
            error: None,
            submitted: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WarningConfirmState {
    pub message: String,
    pub selected_option: WarningOption,
    pub back_to_product: bool, // true → Back 返回产品列表；false → 返回 EnvVarSelection
    pub error: Option<String>,
    pub force_local: bool, // true → Continue 时强制本地写入（跳过 server 委托）
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum WarningOption {
    Continue,
    Back,
}

#[derive(Debug, Clone)]
pub struct VerifyingState {
    pub product_name: String,
    pub env_var: Option<String>,
    pub models: Vec<String>,
    pub force_local: bool, // true → 强制本地写入（跳过 server 委托）
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

// ── Product List Builder ─────────────────────────────────────────────────

pub fn build_product_list() -> Vec<ProductItem> {
    let entries = crate::catalog::built_in_providers();

    // 尝试加载 OAuth 账号
    let accounts = crate::auth::load_oauth_accounts(&crate::auth::default_state_path())
        .ok()
        .map(|(a, _)| a);

    let mut items: Vec<ProductItem> = entries
        .iter()
        .map(|entry| {
            let auth_type = match entry.provider.auth_config(entry.id) {
                Ok(crate::config::AuthConfig::OpenaiOauth { .. }) => ProviderAuthType::OpenaiOauth,
                Ok(crate::config::AuthConfig::AntigravityOauth { .. }) => {
                    ProviderAuthType::AntigravityOauth
                }
                Ok(crate::config::AuthConfig::ApiKeyEnv { .. }) => ProviderAuthType::ApiKey,
                Ok(crate::config::AuthConfig::None) => ProviderAuthType::None,
                Err(_) => ProviderAuthType::ApiKey,
            };

            // 获取认证状态
            let auth_status = if let Some(ref accs) = accounts {
                match crate::auth::get_provider_auth_status(&entry.provider, entry.id, accs) {
                    crate::auth::ProviderAuthStatus::Ready => Some("✓ Ready".to_string()),
                    crate::auth::ProviderAuthStatus::NotLoggedIn => {
                        Some("⚠ Not logged in".to_string())
                    }
                    crate::auth::ProviderAuthStatus::Expired => Some("⚠ Login expired".to_string()),
                    crate::auth::ProviderAuthStatus::MissingKey(env) => {
                        Some(format!("✗ Missing {}", env))
                    }
                }
            } else {
                // 文件不存在，OAuth provider 显示未登录
                match entry.provider.auth_config(entry.id) {
                    Ok(crate::config::AuthConfig::OpenaiOauth { .. })
                    | Ok(crate::config::AuthConfig::AntigravityOauth { .. }) => {
                        Some("⚠ Not logged in".to_string())
                    }
                    Ok(crate::config::AuthConfig::ApiKeyEnv { env }) => {
                        if std::env::var(&env).is_ok() {
                            Some("✓ Ready".to_string())
                        } else {
                            Some(format!("✗ Missing {}", env))
                        }
                    }
                    _ => None,
                }
            };

            ProductItem {
                id: entry.id.to_string(),
                display_name: format_display_name(entry.id),
                auth_type,
                endpoint_count: entry.provider.endpoints().len(),
                product_kind: if matches!(
                    entry.provider.auth_config(entry.id),
                    Ok(crate::config::AuthConfig::OpenaiOauth { .. })
                        | Ok(crate::config::AuthConfig::AntigravityOauth { .. })
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
                auth_status,
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
        auth_status: None,
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
        "google-antigravity" => "Google Antigravity".to_string(),
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
fn check_oauth_token(token_type: &str, account_id: &str) -> Option<bool> {
    let path = crate::auth::default_state_path();
    let (accounts, _skipped) = crate::auth::load_oauth_accounts(&path).ok()?;
    match token_type {
        "openai" => accounts
            .openai
            .get(account_id)
            .map(|entry| !entry.is_expired()),
        "antigravity" => accounts
            .antigravity
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

    let mut providers = Vec::new();

    for (name, provider) in &config.providers {
        // 确定产品名（通过匹配 endpoint URL 或标记为 Custom）
        let product = identify_product(provider);

        // 确定认证类型
        let auth_type = match &provider.auth {
            Some(crate::config::AuthConfig::ApiKeyEnv { .. }) => "ApiKey".to_string(),
            Some(crate::config::AuthConfig::OpenaiOauth { .. }) => "OpenAI OAuth".to_string(),
            Some(crate::config::AuthConfig::AntigravityOauth { .. }) => {
                "Antigravity OAuth".to_string()
            }
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
                    match check_oauth_token("openai", account_id) {
                        Some(true) => ProviderStatus::Ok,
                        Some(false) => ProviderStatus::Warning, // expired
                        None => ProviderStatus::Error,          // not found
                    }
                }
                Some(crate::config::AuthConfig::AntigravityOauth { account }) => {
                    let account_id = account.as_deref().unwrap_or(name);
                    match check_oauth_token("antigravity", account_id) {
                        Some(true) => ProviderStatus::Ok,
                        Some(false) => ProviderStatus::Warning,
                        None => ProviderStatus::Error,
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
        if provider.antigravity.is_some() {
            protocols.push("antigravity".to_string());
        }

        providers.push(ProviderListItem {
            name: name.clone(),
            product,
            auth_type,
            status,
            protocols,
        });
    }

    // 按名称排序
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    providers
}

/// 识别 provider 属于哪个产品
fn identify_product(provider: &crate::config::ProviderConfig) -> String {
    // 优先使用 product 字段（Phase 1 已添加）
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
        if url.contains("googleapis.com") {
            return "Google Antigravity".to_string();
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
    fn identify_product_matches_anthropic_only_provider() {
        // 纯 Anthropic provider（无 openai_chat）不应再被标为 Custom
        let provider = ProviderConfig {
            anthropic: Some(endpoint("https://api.anthropic.com/v1/messages")),
            ..Default::default()
        };
        assert_eq!(identify_product(&provider), "Anthropic API");
    }

    #[test]
    fn identify_product_matches_antigravity_only_provider() {
        let provider = ProviderConfig {
            antigravity: Some(endpoint("https://antigravity-pa.googleapis.com/...")),
            ..Default::default()
        };
        assert_eq!(identify_product(&provider), "Google Antigravity");
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
        };
        assert_eq!(state.selected_target(), Some("deepseek-2"));
    }
}
