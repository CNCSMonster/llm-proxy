# Rust v2 TUI Detailed Design

Status: ✅ implemented（TUI 功能已全部落地：产品选择向导、Custom Provider 编辑器、Provider 管理面板、Usage TUI；见 ROADMAP 已完成任务 4/9）
Scope: ratatui + crossterm TUI implementation for connect/provider/model
Last updated: 2026-07-27
References: `../../spec.md` (§12.4, at project root), Go v1 `internal/connect/tui.go`

## 1. Overview

This document specifies the detailed TUI implementation design. It covers:

- Component architecture and event loop
- Screen-by-screen rendering specification
- Key binding reference
- State machine with all transitions
- Data flow between TUI and backend
- Phase-by-phase implementation plan

The TUI follows the same Model-Update-View pattern as the Go v1 bubbletea implementation.

## 2. Architecture

### 2.1 Module Structure

```
src/tui/
  mod.rs           # Event loop, terminal setup/cleanup, run() entry point
  model.rs         # AppModel, Screen enum, per-screen state structs
  update.rs        # handle_key() dispatch, state transitions, side-effect commands
  view.rs          # render() per-screen draw functions
  widgets.rs       # Shared widget builders: product_list, env_list, model_list, help_bar, error_banner
  style.rs         # Color palette, style constants (green/red/yellow/dim/bold)
```

### 2.2 Event Loop

```rust
pub async fn run(config_path: PathBuf, entry: EntryMode) -> Result<()> {
    // 1. Setup terminal
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    // 2. Initialize state
    let mut app = AppModel::new(config_path, entry);

    // 3. Main loop
    let res = loop {
        // Render
        terminal.draw(|f| view::render(f, &app))?;

        // Check quit
        if app.screen == Screen::Quit { break Ok(()); }

        // Run async commands (OAuth polling, connectivity probe)
        if let Some(cmd) = app.pending_command.take() {
            match cmd {
                Command::VerifyConnectivity { product, env_var, models } => {
                    let result = connectivity::probe(&product, &env_var, &models).await;
                    app.handle_verify_result(result);
                }
                Command::OAuthPoll { session } => {
                    let result = session.poll().await;
                    app.handle_oauth_result(result);
                }
                // ...
            }
            continue; // Re-render immediately after command completes
        }

        // Block on next key event
        if let Ok(Event::Key(key)) = event::read() {
            update::handle_key(&mut app, key);
        }
    };

    // 4. Cleanup
    crossterm::execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    crossterm::terminal::disable_raw_mode()?;
    res
}
```

### 2.3 Command Pattern

Long-running async operations (network probes, OAuth polling) are NOT performed
inside `handle_key`. Instead, `handle_key` sets `app.pending_command` to a
`Command` enum variant. The event loop checks for pending commands after each
render, executes them asynchronously, and feeds the result back through
dedicated handler methods.

```rust
pub enum Command {
    /// Verify connectivity for a selected product/env/model combination.
    VerifyConnectivity {
        product: CatalogProduct,
        env_var: Option<String>,
        models: Vec<String>,
    },
    /// Start OAuth device code login on remote thread and poll for completion.
    OAuthStartLogin {
        provider_name: String,
        product: CatalogProduct,
    },
    /// Poll OAuth session (called every tick from timer).
    OAuthPoll {
        session: Arc<OAuthLoginSession>,
    },
    /// Discover models from Ollama daemon.
    DiscoverOllamaModels {
        base_url: String,
    },
    /// Fetch model templates for a catalog product.
    FetchModelTemplates {
        provider_id: String,
    },
}
```

### 2.4 Async Runtime

The TUI runs on `tokio` (already used by the rest of llm-proxy). Network
commands use `tokio::spawn` or direct `.await` depending on the operation
duration:

- Connectivity probe: direct `.await` (≤10s timeout, acceptable)
- OAuth polling: `tokio::spawn` + ticker every 3s
- Model template fetch: direct `.await` (single HTTP call)
- Ollama model discovery: direct `.await` (single HTTP call)

## 3. State Machine

### 3.1 Screen Enum

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    /// Entry: list of catalog products + "Custom provider" item
    ProductSelection(ProductSelectionState),

    /// API key product: choose env var from scanned environment
    EnvVarSelection(EnvVarSelectionState),

    /// Mature product or Ollama: multi-select model list
    ModelSelection(ModelSelectionState),

    /// Custom provider: type a unique provider name
    CustomProviderName(CustomProviderNameState),

    /// Custom provider: type a complete upstream URL
    CustomProviderEndpoint(CustomProviderEndpointState),

    /// Custom provider: choose protocol type (openai-chat/responses/anthropic)
    CustomProviderType(CustomProviderTypeState),

    /// OAuth product: choose provider name (recommended or custom)
    OAuthName(OAuthNameState),

    /// OAuth product: device code flow active
    OAuthDeviceCode(OAuthDeviceCodeState),

    /// Antigravity product: browser login + paste code
    AntigravityLogin(AntigravityLoginState),

    /// Verification warning: probe failed, user chooses continue/retry
    WarningConfirm(WarningConfirmState),

    /// Verification in progress (spinner shown)
    Verifying(VerifyingState),

    /// OAuth overwrite warning: provider already exists with this name
    OAuthOverwrite(OAuthOverwriteState),

    /// Results display: list of configured providers/models
    Done(DoneState),

    /// Terminal quit requested
    Quit,
}
```

### 3.2 State Transitions

```
ProductSelection ──Enter──→ (mature product)
    │                         ├── API key type → EnvVarSelection
    │                         ├── OAuth type → OAuthName
    │                         └── Ollama type → Verifying → ModelSelection
    │
    └──Enter──→ (custom provider)
                 ├── CustomProviderType → CustomProviderName → CustomProviderEndpoint
                 └──→ EnvVarSelection → Verifying → Done

EnvVarSelection ──Enter──→ (selected env var)
    │                        └──→ Verifying
    │                              ├── OK → ModelSelection (mature product)
    │                              │       or Done (custom provider)
    │                              └── FAIL → WarningConfirm
    │                                          ├── Continue → Done
    │                                          └── Back → EnvVarSelection
    └──'s' key──→ (skip env var)
                   └──→ same as Enter

ModelSelection ──Enter──→ (at least one selected)
                           └──→ Done

OAuthName ──Enter──→ (name chosen)
    │                  └──→ OAuthDeviceCode or AntigravityLogin
    │                        ├── Success → EnvVarSelection (or ModelSelection)
    │                        └── Timeout/Error → OAuthName

All states ──Esc──→ parent state
All states ──q───→ ProductSelection (or Quit if already there)
```

### 3.3 Per-Screen State Structs

```rust
#[derive(Debug, Clone)]
pub struct ProductSelectionState {
    pub items: Vec<ProductItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnvVarSelectionState {
    pub items: Vec<EnvItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub error: Option<String>,
    pub product_name: String,
}

#[derive(Debug, Clone)]
pub struct ModelSelectionState {
    pub items: Vec<ModelItem>,
    pub cursor: usize,
    pub filter: String,
    pub filter_active: bool,
    pub selected: HashSet<String>,
    pub error: Option<String>,
    pub product_name: String,
    pub configured_models: HashSet<String>, // already in config
}

#[derive(Debug, Clone)]
pub struct CustomProviderNameState {
    pub input: String,
    pub cursor_pos: usize,
    pub protocol: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CustomProviderEndpointState {
    pub name: String,
    pub protocol: String,
    pub input: String,
    pub cursor_pos: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CustomProviderTypeState {
    pub types: Vec<ProtocolType>,
    pub cursor: usize,
    pub error: Option<String>,
}

// Protocol types for custom providers
pub struct ProtocolType {
    pub id: String,          // "openai-chat", "openai-responses", "anthropic"
    pub display_name: String,// "OpenAI Chat 格式"
    pub description: String, // "兼容 /v1/chat/completions 端点"
}

#[derive(Debug, Clone)]
pub struct OAuthNameState {
    pub recommended_name: String,
    pub selected_option: NameOption,  // Recommended or Custom
    pub input: String,               // custom name input
    pub input_active: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NameOption { Recommended, Custom }

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
    pub copied: bool,
    pub input: String,       // authorization code input
    pub input_active: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WarningConfirmState {
    pub message: String,
    pub selected_option: WarningOption,  // Continue or Back
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WarningOption { Continue, Back }

#[derive(Debug, Clone)]
pub struct VerifyingState {
    pub product_name: String,
    pub env_var: Option<String>,
    pub models: Vec<String>,
    pub spinner_frame: usize,
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
    pub show_back_hint: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigResult {
    pub provider: String,
    pub success: bool,
    pub message: String,
}
```

### 3.4 AppModel (Top-Level State)

```rust
pub struct AppModel {
    pub config_path: PathBuf,
    pub screen: Screen,
    pub pending_command: Option<Command>,
    /// Results accumulated during a single connect session
    pub session_results: Vec<ConfigResult>,
    /// Chosen product (set on ProductSelection confirm)
    pub chosen_product: Option<CatalogProduct>,
    /// Chosen env var (set on EnvVarSelection confirm)
    pub chosen_env_var: Option<String>,
    /// Chosen models (set on ModelSelection confirm)
    pub chosen_models: Vec<String>,
    /// Custom provider fields
    pub custom_type: Option<String>,
    pub custom_name: Option<String>,
    pub custom_endpoint: Option<String>,
    /// OAuth state
    pub oauth_provider_name: Option<String>,
    pub oauth_session: Option<Arc<OAuthLoginSession>>,
    /// Double-ESC protection
    pub last_esc: Option<Instant>,
    pub esc_hint_visible: bool,
    /// Terminal dimensions (updated on resize)
    pub width: u16,
    pub height: u16,
}
```

## 4. Rendering Design

### 4.1 Global Layout

Every screen shares this layout:

```
┌─ Title Bar ──────────────────────────────────────────────┐
│  🚀 llm-proxy 配置向导                                    │
├─ Content Area ────────────────────────────────────────────┤
│                                                          │
│  (varies per screen)                                     │
│                                                          │
├─ Error Banner (only when error.is_some()) ───────────────┤
│  ⚠️  error message                                       │
├─ Help Bar ───────────────────────────────────────────────┤
│  [Enter] Confirm  [Esc] Back  [q] Quit  [/] Search       │
└──────────────────────────────────────────────────────────┘
```

**Implementation**:

```rust
pub fn render(frame: &mut Frame, app: &AppModel) {
    let area = frame.area();

    // Split vertically: title (1 line), content (fill), help (1 line)
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ]).split(area);

    // Title
    frame.render_widget(
        Paragraph::new("🚀 llm-proxy 配置向导").style(TITLE_STYLE),
        chunks[0],
    );

    // Content (per-screen)
    match &app.screen {
        Screen::ProductSelection(s) => render_product_selection(frame, chunks[1], s),
        Screen::EnvVarSelection(s) => render_env_var_selection(frame, chunks[1], s),
        Screen::ModelSelection(s) => render_model_selection(frame, chunks[1], s),
        // ... (other screens)
        Screen::Done(s) => render_done(frame, chunks[1], s, app),
        _ => {}
    }

    // Error banner (if any)
    if let Some(error) = app.current_error() {
        frame.render_widget(
            Paragraph::new(format!("⚠️  {}", error)).style(ERROR_STYLE),
            chunks[2], // overlaps with help — render conditionally
        );
    }

    // Help bar
    frame.render_widget(
        Paragraph::new(app.help_text()).style(DIM_STYLE),
        chunks[2],
    );
}
```

### 4.2 Product Selection Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  ┌ Select Product ──────────────────────────────────┐   │
│  │                                                    │   │
│  │  DeepSeek API (API Key)                           │   │
│  │  按量付费 · 3 个 endpoint                          │   │
│  │                                                    │   │
│  │  Bailian Coding Plan (API Key)                    │   │
│  │  订阅套餐 · 2 个 endpoint                          │   │
│  │                                                    │   │
│  │▶ MiMo PAYG (API Key)                              │   │
│  │  按量付费 · 3 个 endpoint                          │   │
│  │                                                    │   │
│  │  ...                                               │   │
│  │                                                    │   │
│  │  Custom provider...                                │   │
│  │  手动配置 endpoint、API Key 和模型                 │   │
│  │                                                    │   │
│  └────────────────────────────────────────────────────┘   │
│                                                          │
│  Filter: │deep│█                                          │
│                                                          │
│  3/21 items                                              │
└──────────────────────────────────────────────────────────┘
```

**Implementation details**:
- Use `ratatui::widgets::List` with custom `ListItem` rendering
- Each item: primary text (product name), secondary text (auth type + endpoint count, dim style)
- "Custom provider" item always last, dimmed background
- Filter bar appears when `/` is pressed, shows current filter string
- Status bar at bottom shows "3/21 items" or "3/21 (filtered)"

```rust
fn render_product_selection(frame: &mut Frame, area: Rect, state: &ProductSelectionState) {
    let items: Vec<ListItem> = state.items.iter().enumerate().map(|(i, item)| {
        let style = if i == state.cursor { HIGHLIGHT_STYLE } else { Style::default() };
        ListItem::new(Line::from(vec![
            Span::styled(&item.display_name, style),
            Span::styled(format!(" ({})", item.auth_label), DIM_STYLE),
            Span::styled(format!(" · {} endpoints", item.endpoint_count), DIM_STYLE),
        ]))
    }).collect();

    let list = List::new(items)
        .block(Block::bordered().title(" Select Product "))
        .highlight_style(SELECTED_STYLE)
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut state.list_state());
}
```

### 4.3 Model Selection Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  ┌ Select Models — Bailian Coding Plan ─────────────┐   │
│  │                                                    │   │
│  │  [✓] qwen3.7-plus-bailian-lp                      │   │
│  │      ctx=1.0M  max_out=65K  image ✓                │   │
│  │                                                    │   │
│  │  [ ] qwen3.6-plus-bailian-lp                      │   │
│  │      ctx=1.0M  max_out=65K  image ✓                │   │
│  │                                                    │   │
│  │  [★] kimi-k2.5-bailian-lp  (已配置)               │   │
│  │      ctx=262K  max_out=98K  image ✓                │   │
│  │                                                    │   │
│  └────────────────────────────────────────────────────┘   │
│                                                          │
│  已选择 1/6 个模型    [Space] 切换  [a] 全选  [Enter] 确认 │
└──────────────────────────────────────────────────────────┘
```

**Implementation details**:
- Each item shows: `[✓]` or `[ ]` or `[★]` (already configured), model name, context/max_output, capabilities
- Space toggles selection
- `a` selects all / deselects all
- At least one model must be selected before Enter is accepted
- Configured models are pre-selected (`[★]`)

```rust
fn render_model_selection(frame: &mut Frame, area: Rect, state: &ModelSelectionState) {
    let items: Vec<ListItem> = state.items.iter().enumerate().map(|(i, item)| {
        let marker = if state.configured_models.contains(&item.id) {
            Span::styled("[★]", SUCCESS_STYLE)
        } else if state.selected.contains(&item.id) {
            Span::styled("[✓]", SUCCESS_STYLE)
        } else {
            Span::styled("[ ]", DIM_STYLE)
        };

        let name = Span::styled(&item.display_name, Style::default());
        let details = Span::styled(
            format!("ctx={}  max_out={}  image={}",
                format_tokens(item.context_window),
                format_tokens(item.max_output),
                if item.supports_image { "✓" } else { "✗" }
            ),
            DIM_STYLE,
        );

        let style = if i == state.cursor { HIGHLIGHT_STYLE } else { Style::default() };
        ListItem::new(Line::from(vec![marker, " ".into(), name, "  ".into(), details]))
            .style(style)
    }).collect();

    let list = List::new(items)
        .block(Block::bordered().title(format!(" Select Models — {} ", state.product_name)))
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut state.list_state());
}
```

### 4.4 Verification Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  正在验证配置...                                          │
│                                                          │
│  Provider:  deepseek                                     │
│  Endpoint:  https://api.deepseek.com/chat/completions    │
│  Model:     deepseek-v4-flash-lp                         │
│                                                          │
│  ⠋ Verifying...                                          │
│                                                          │
│  (this may take a few seconds)                           │
└──────────────────────────────────────────────────────────┘
```

### 4.5 Warning Confirm Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  ⚠️  验证失败                                            │
│                                                          │
│  Connection to https://api.example.com failed:            │
│  HTTP 401 Unauthorized                                   │
│                                                          │
│  API key reference will still be saved, but the           │
│  provider may not work until the key is corrected.        │
│                                                          │
│  ▶ 仍继续写入配置                                        │
│    返回重新选择                                          │
│                                                          │
│  [↑/↓] 移动    [Enter] 确认                              │
└──────────────────────────────────────────────────────────┘
```

### 4.6 Done Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  ✅ 配置完成                                             │
│                                                          │
│  Provider:   deepseek           ✓ Configured             │
│  Model:      deepseek-v4-flash-lp  ✓ Bound               │
│  Model:      deepseek-v4-pro-lp    ✓ Bound               │
│                                                          │
│  [Enter] 配置另一个 provider     [q] 退出                 │
└──────────────────────────────────────────────────────────┘
```

### 4.7 OAuth Device Code Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  正在登录 openai-subscription                            │
│                                                          │
│  1. 打开浏览器访问:                                       │
│     https://device.login.openai.com/                     │
│                                                          │
│  2. 输入以下代码:                                         │
│     XXXX-YYYY                                            │
│                                                          │
│  [c] 复制代码到剪贴板  ✓                                 │
│  [o] 打开浏览器                                          │
│                                                          │
│  ✅ 代码已复制到剪贴板                                   │
│                                                          │
│  等待授权中... ⠋ (剩余 14m 32s)                          │
│                                                          │
│  [Esc] 跳过    [s] 停止全部                              │
└──────────────────────────────────────────────────────────┘
```

### 4.8 Antigravity Login Screen

```
┌── Content ───────────────────────────────────────────────┐
│                                                          │
│  正在登录 antigravity (Google Antigravity)               │
│                                                          │
│  1. 在浏览器中打开以下地址:                               │
│     https://accounts.google.com/o/oauth2/auth?...        │
│     ...(long URL wrapped)                                │
│     ✓ 完整 URL 已自动复制到剪贴板                        │
│                                                          │
│  [c] 复制 URL  ✓    [o] 打开浏览器                       │
│                                                          │
│  2. 登录 Google 并完成授权                                │
│                                                          │
│  3. 将页面显示的授权码粘贴到下面:                          │
│                                                          │
│   ┌──── Paste Code ─────────────────────────────────┐    │
│   │ │4/0Ad...                                       │    │
│   └─────────────────────────────────────────────────┘    │
│                                                          │
│  [Enter] 提交    [Esc] 返回                              │
└──────────────────────────────────────────────────────────┘
```

## 5. Key Bindings Reference

### 5.1 Global Keys

| Key | Context | Action |
|-----|---------|--------|
| `Esc` | Any screen | Return to parent screen |
| `Esc` × 2 | Any screen, within 500ms | Quit |
| `q` | ProductSelection | Quit |
| `q` | Any other screen | Return to ProductSelection |
| `Ctrl+C` | Any screen | Quit |
| `?` | Any screen | Show help overlay |

### 5.2 List Navigation

| Key | Action |
|-----|--------|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `PgUp` | Page up (10 items) |
| `PgDn` | Page down (10 items) |
| `Home` / `g g` | Jump to first item |
| `End` / `G` | Jump to last item |
| `/` | Toggle filter mode |
| `Enter` | Confirm selection |

### 5.3 Model Selection

| Key | Action |
|-----|--------|
| `Space` | Toggle current item selection |
| `a` | Select all / Deselect all |
| `Enter` | Confirm (requires at least 1 selected) |
| `Esc` / `q` | Return without confirming |

### 5.4 Text Input

| Key | Action |
|-----|--------|
| `Enter` | Confirm input |
| `Esc` | Cancel / Return (blur input first if focused) |
| `←` / `→` | Move cursor |
| `Backspace` | Delete before cursor |
| `Delete` | Delete after cursor |
| `Ctrl+U` | Clear line |
| `Ctrl+W` | Delete word backwards |

### 5.5 OAuth/Antigravity

| Key | Action |
|-----|--------|
| `c` | Copy code/URL to clipboard |
| `o` | Open browser (xdg-open / open / start) |
| `s` | Stop polling / Cancel login |
| `Esc` | Skip back to name selection |

### 5.6 Warning Confirm

| Key | Action |
|-----|--------|
| `Enter` | Confirm selected option |
| `↑/↓` / `j/k` | Switch between Continue / Back |
| `Esc` / `q` | Equivalent to Back |

## 6. Data Flow

### 6.1 Product Catalog

The product list is built from `crate::catalog::built_in_providers()`.

```rust
pub struct ProductItem {
    pub id: String,              // provider ID, e.g. "deepseek"
    pub display_name: String,    // e.g. "DeepSeek API"
    pub auth_label: String,      // "API Key", "OAuth", "Local"
    pub endpoint_count: usize,   // number of protocol endpoints
    pub product_kind: String,    // "payg", "subscription", "local"
    pub models: Vec<ModelTemplate>,
}

pub struct ModelTemplate {
    pub id: String,              // frontend model ID
    pub display_name: String,    // upstream model name
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub supports_image: bool,
}
```

### 6.2 Configuration Write

The final step calls the existing `connect::add_provider_with_models`:

```rust
// After verification succeeds:
connect::add_provider_with_models(
    &app.config_path,
    &product.id,
    env_var,          // Option<String> — None for OAuth/skip
    no_api_key,       // true for OAuth/skip
    provider_type,    // None for catalog products
    endpoint_url,     // None for catalog products
    Some(&models),    // &[String] — model template IDs
).await?;
```

### 6.3 Clipboard and Browser

Use OS-appropriate commands:

```rust
fn copy_to_clipboard(text: &str) -> Result<bool> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Linux Wayland
        std::process::Command::new("wl-copy").arg(text).status().is_ok()
    } else {
        // Linux X11 / macOS
        std::process::Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn()
            .and_then(|child| child.stdin.unwrap().write_all(text.as_bytes()))
            .is_ok()
    }
    // macOS: pbcopy, Windows: clip
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn()?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd").args(["/c", "start", url]).spawn()?;
    Ok(())
}
```

### 6.4 Connectivity Probe

```rust
async fn probe_connectivity(
    product: &CatalogProduct,
    env_var: Option<&str>,
    models: &[String],
) -> Result<Vec<ProbeResult>> {
    let cfg = build_temp_config(product, env_var);
    let client = reqwest::Client::new();

    let mut results = Vec::new();
    for model in models {
        let url = format!("{}/openai/v1/chat/completions", cfg.server.listen);
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
        });
        let resp = client.post(&url).json(&body).timeout(Duration::from_secs(10)).send().await?;
        results.push(ProbeResult {
            model: model.to_string(),
            ok: resp.status().is_success(),
            status: resp.status().as_u16(),
            error: if resp.status().is_success() { None } else { Some(resp.text().await.unwrap_or_default()) },
        });
    }
    Ok(results)
}
```

## 7. Style Constants

```rust
use ratatui::style::{Color, Style, Modifier};

pub const TITLE_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
pub const SELECTED_STYLE: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
pub const HIGHLIGHT_STYLE: Style = Style::new().bg(Color::DarkGray);
pub const DIM_STYLE: Style = Style::new().fg(Color::DarkGray);
pub const ERROR_STYLE: Style = Style::new().fg(Color::Red);
pub const SUCCESS_STYLE: Style = Style::new().fg(Color::Green);
pub const WARNING_STYLE: Style = Style::new().fg(Color::Yellow);
pub const INSTRUCTION_STYLE: Style = Style::new().fg(Color::Gray);
pub const INPUT_STYLE: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
pub const BORDER_STYLE: Style = Style::new().fg(Color::DarkGray);
```

## 8. Implementation Plan

### Phase 1: Foundation (2-3h)

**Goal**: Terminal setup, event loop, Screen enum, screen switching.

- [ ] Add `ratatui` and `crossterm` to Cargo.toml
- [ ] Create `src/tui/mod.rs` with `run()` entry point
- [ ] Create `src/tui/style.rs` with color constants
- [ ] Create `src/tui/model.rs` with `AppModel`, `Screen` enum
- [ ] Create `src/tui/update.rs` with `handle_key()` dispatcher
- [ ] Create `src/tui/view.rs` with `render()` and `render_product_selection()`
- [ ] Create `src/tui/widgets.rs` with `help_bar()` builder
- [ ] Implement `ProductSelection → Quit` transition
- [ ] Wire `connect` CLI to use new TUI (replace `prompt()` in `connect_wizard`)

**Acceptance**: `llm-proxy connect` opens fullscreen TUI, shows product list, `q` quits.

### Phase 2: Product Selection & Env Var (2-3h)

**Goal**: Full product selection → env var selection flow for API-key products.

- [ ] Implement `EnvVarSelection` screen (scan `std::env::vars()`)
- [ ] Implement `CustomProviderType → CustomProviderName → CustomProviderEndpoint` flow
- [ ] Implement filter mode (`/` key) on list screens
- [ ] Implement vim-style navigation (`j`/`k`)
- [ ] Wire env var selection to `connect::add_provider_with_models`

**Acceptance**: Can select DeepSeek, pick `DEEPSEEK_API_KEY`, write config.

### Phase 3: Model Selection & Verification (2-3h)

**Goal**: Model selection screen, connectivity probe, warning confirmation.

- [ ] Implement `ModelSelection` screen with multi-select
- [ ] Implement `Verifying` screen with spinner
- [ ] Implement `WarningConfirm` screen
- [ ] Implement `Done` screen
- [ ] Add connectivity probe command
- [ ] Fetch model templates from catalog (per-product model list)

**Acceptance**: Can select Bailian Coding Plan, multi-select models, verify connectivity.

### Phase 4: OAuth (3-4h)

**Goal**: OAuth device code flow, Antigravity browser login.

- [ ] Implement `OAuthName` screen
- [ ] Implement `OAuthDeviceCode` screen
- [ ] Implement `AntigravityLogin` screen
- [ ] Implement `OAuthOverwrite` screen
- [ ] Add OAuth command dispatching
- [ ] Add clipboard integration (`xclip`/`wl-copy`/`pbcopy`)
- [ ] Add browser open integration (`xdg-open`/`open`/`start`)

**Acceptance**: Can login to OpenAI Subscription, complete device code flow.

### Phase 5: Polish (1-2h)

**Goal**: Edge cases, error handling, visual polish.

- [ ] Double-ESC quit protection
- [ ] Terminal resize handling
- [ ] Color-blind friendly mode (`NO_COLOR` env support)
- [ ] Responsive layout (min 80×24 terminal)
- [ ] Filter persistence across screen transitions
- [ ] Error recovery (network timeout, invalid input)

**Acceptance**: All flows work smoothly, no crashes on edge cases.

### Phase 6: Provider TUI & Model TUI (Future)

**Goal**: Full provider management TUI (§12.4 Layer 1/2) and model TUI.

- [ ] Provider List screen
- [ ] Provider Detail screen
- [ ] Usage display (ChatGPT subscription)
- [ ] Model TUI integration

## 9. Testing Strategy

### 9.1 Unit Tests

```rust
// model.rs
#[test]
fn test_screen_transitions() { ... }
#[test]
fn test_model_selection_toggle() { ... }
#[test]
fn test_filter_items() { ... }

// update.rs
#[test]
fn test_esc_returns_to_parent() { ... }
#[test]
fn test_enter_confirms_product() { ... }

// view.rs
#[test]
fn test_render_product_list_output() { ... }
```

### 9.2 Integration Tests

```rust
#[test]
fn test_full_connect_flow_api_key() {
    // Setup: mock catalog, mock env vars, mock connectivity probe
    // Start TUI with synthetic key events
    // Verify config.toml written correctly
}

#[test]
fn test_full_connect_flow_oauth() { ... }
#[test]
fn test_custom_provider_flow() { ... }
```

### 9.3 Manual Smoke Tests

- `llm-proxy connect` → select DeepSeek → pick `DEEPSEEK_API_KEY` → select models → verify → done
- `llm-proxy connect` → select "Custom provider" → openai-chat → name → endpoint → success
- `llm-proxy connect` → select OpenAI Subscription → OAuth login → success

## 10. Bug Fixes & Design Updates (2026-08-07)

### 10.1 TUI 写入路径统一（P0）

**问题**：TUI 的 `write_provider_config`、editor save、provider remove 三个写入路径各自手动实现 `detect_server` + 分支逻辑，缺少 `HeldByServer` 重试回退。当 Server 运行但 `detect_server` 返回 `None`（如 ping 超时）时，TUI 写入会报"写入权被占用"。

**修复**：所有 TUI 写入路径统一使用 `with_cli_write_lock_or_delegate`（§15.2 完整 5 步流程），与 CLI 路径保持一致。

**修改文件**：
- `src/tui/mod.rs` — `write_provider_config` 改用 `with_cli_write_lock_or_delegate`
- `src/tui/update/editor.rs` — `execute_edit_save` 改用 `with_cli_write_lock_or_delegate`
- `src/tui/update/provider_mgmt.rs` — delete handler 改用 `with_cli_write_lock_or_delegate`

### 10.2 WarningConfirm Continue 无限循环修复（P0.5）

**问题**：当 `write_provider_config` 失败后进入 `WarningConfirm`，按 Enter 选择 "Continue anyway" 会重新进入 `Verifying` 再次尝试写入，再次失败，形成无限循环。

**修复**：
- `WarningConfirmState` 和 `VerifyingState` 新增 `force_local: bool` 字段
- 首次 Continue：设置 `force_local = true`，进入 `Verifying` 使用本地写入（跳过 server 委托）
- 二次失败 Continue：直接进入 `Done` 屏幕显示错误，打破循环

**新增函数**：
- `write_provider_config_local` — 仅本地写入，不尝试 server 委托

### 10.3 Ollama TUI None auth 分支（P1）

**问题**：Ollama 的 `auth_type` 为 `None`，但 TUI 的 `select_product()` 只特殊处理了 OAuth 类型，`None` 落入 `_ => {}` 分支后继续走到 `EnvVarSelection`，用户被迫选择环境变量。

**修复**：在 `select_product()` 中增加 `ProviderAuthType::None` 分支：
- 跳过 env var 选择
- 直接进入 model selection 或 verifying（取决于是否有模型模板）

### 10.4 TUI 搜索支持 Ctrl+N/P（P2）

**问题**：搜索模式下只支持 Up/Down/k/j 移动，不支持 Ctrl+N/P（Emacs 风格导航）。

**修复**：在 `handle_env_keys`、`handle_product_keys`、`handle_model_keys` 的搜索模式分支中增加 `KeyCode::Char('n') + CONTROL` 和 `KeyCode::Char('p') + CONTROL` 处理。

**注意**：使用 guard 条件区分 `k`/`j`（无修饰符时为导航）和 `Ctrl+K`/`Ctrl+J`（如有其他用途）。

### 10.5 Key Binding Reference Update

搜索模式下的完整按键绑定：

| Key | Action |
|-----|--------|
| `↑` / `k` | Move up |
| `↓` / `j` | Move down |
| `Ctrl+P` | Move up (Emacs style) |
| `Ctrl+N` | Move down (Emacs style) |
| `Enter` | Confirm selection |
| `Esc` | Deactivate filter |
| Other chars | Append to filter |
| `Backspace` | Delete from filter |

Product selection 特殊按键（搜索模式下）：

| Key | Action |
|-----|--------|
| `↑` / `↓` / `j` / `k` | Navigate |
| `Ctrl+P` / `Ctrl+N` | Navigate |
| `Enter` | Select product (go to next screen) |
| `Esc` | Deactivate filter |

Model selection 特殊按键（搜索模式下）：

| Key | Action |
|-----|--------|
| `Space` | Toggle model selection |
| `a` | Select all / Deselect all |
| `Enter` | Confirm selection |

### 10.6 Provider TUI 刷新保持光标位置（2026-08-08）

**问题**：Provider 管理面板按 `r` 刷新时，光标会跳到列表顶部（`cursor = 0`），丢失当前选中的 provider。

**修复**：刷新前保存当前选中的 provider 名称，刷新后尝试定位到同名 provider；如果找不到才重置到 0。

**修改文件**：`src/tui/update/provider_mgmt.rs` — `handle_provider_management_keys` 的 `KeyCode::Char('r')` 分支。

### 10.7 Provider TUI 刷新自动刷新 OAuth Token（2026-08-08）

**问题**：Provider 管理面板按 `r` 刷新时，只重新加载配置文件，不自动刷新过期的 OAuth token。用户看到 ⚠（token 过期）后按 `r`，仍显示 ⚠，需要手动执行 `provider refresh` 或按 `l` 登录。

**设计决策**：`[r]` 应该自动刷新过期的 access_token（如果 refresh_token 有效），用户只需按一次 `[r]` 就能看到最新状态。

**行为规范**：
| 场景 | 行为 |
|------|------|
| access_token 未过期 | 只重新加载配置 |
| access_token 过期 + refresh_token 有效 | 自动刷新 + 显示 ✓ |
| access_token 过期 + refresh_token 也过期 | 显示错误："请重新登录" |
| 网络请求失败 | 显示错误："刷新失败，请检查网络" |

**修改文件**：`src/tui/update/provider_mgmt.rs` — `handle_provider_management_keys` 的 `KeyCode::Char('r')` 分支。
