//! Admin API client for CLI-to-server communication.
//!
//! The CLI uses this client to detect whether a local server is running
//! and delegate operations via the Admin API when available.

#![allow(dead_code)] // Client methods available as CLI commands integrate server delegation

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

fn parse_sse_chunk(
    bytes: Result<Bytes, reqwest::Error>,
    buf: &mut String,
) -> Vec<Result<SseEvent>> {
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => return vec![Err(error.into())],
    };
    buf.push_str(&String::from_utf8_lossy(&bytes));
    let mut out = Vec::new();
    while let Some(idx) = buf.find("\n\n") {
        let raw = buf[..idx].to_string();
        *buf = buf[idx + 2..].to_string();
        let mut event = "message".to_string();
        let mut data = String::new();
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            }
            if let Some(v) = line.strip_prefix("data:") {
                data.push_str(v.trim());
            }
        }
        if !data.is_empty() {
            match serde_json::from_str(&data) {
                Ok(data) => out.push(Ok(SseEvent { event, data })),
                Err(e) => out.push(Err(e.into())),
            }
        }
    }
    out
}

/// Convert a streaming reqwest::Response into a Stream of parsed SseEvents.
pub(crate) fn response_to_sse_stream(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<SseEvent>> + Send + 'static {
    let mut buf = String::new();
    resp.bytes_stream()
        .flat_map(move |chunk| futures_util::stream::iter(parse_sse_chunk(chunk, &mut buf)))
}

/// Information about a detected running server.
#[derive(Debug, Clone)]
pub struct ServerConnection {
    pub base_url: String,
    pub server_version: Option<String>,
    /// 该 server 正在使用的 config 身份（`/admin/ping` 返回）。
    /// 写路径据此拒绝委托给"监听同一地址但用的是另一份 config"的 server。
    /// `None` 表示对端没有上报（早于该检查的版本）。
    pub config_identity: Option<String>,
    /// 管理通道 socket 路径（Unix）：所有 admin 操作经此发送（
    /// 全部 `/admin/*` 端点迁入 UDS 管理通道，0600 权限即信任）。
    /// Windows 忽略此字段，管理通道走 localhost TCP（端口从配置读取）。
    #[cfg_attr(windows, allow(dead_code))]
    pub uds_path: std::path::PathBuf,
    /// Windows 管理通道端口（从配置读取，默认 8989）。
    /// Unix 上忽略此字段（走 UDS）。
    #[cfg_attr(unix, allow(dead_code))]
    pub management_port: u16,
}

/// Minimum server version supporting probe selection (§4).
///
/// Dev builds cannot expose a stable feature version. The selector gate therefore accepts them
/// only when both client and server are dev builds; formal clients require this released minimum.
pub const PROBE_SELECTION_MIN_VERSION: &str = "0.3.0";

/// Check whether a CLI/server build pair supports probe selection (§4).
///
/// A dev server has no stable feature version, so it is accepted only when the CLI is also a
/// dev build. A formal CLI must reject a dev server rather than risk an older handler silently
/// ignoring the selector and probing every Provider.
pub fn check_probe_selection_version_for(cli_version: &str, server_version: &str) -> Result<()> {
    let cli_is_dev = cli_version.contains("-dev");
    let server_is_dev = server_version.contains("-dev");
    if cli_is_dev && server_is_dev {
        return Ok(());
    }
    if server_is_dev {
        bail!(
            "server does not support probe selection (formal CLI requires server version >= {PROBE_SELECTION_MIN_VERSION})"
        );
    }
    let parsed = semver::Version::parse(server_version)
        .with_context(|| format!("server returned invalid version {server_version:?}"))?;
    let min = semver::Version::parse(PROBE_SELECTION_MIN_VERSION)
        .expect("PROBE_SELECTION_MIN_VERSION must be valid semver");
    if parsed < min {
        bail!(
            "server does not support probe selection (server version {server_version} < {PROBE_SELECTION_MIN_VERSION})"
        );
    }
    Ok(())
}

/// §13 版本兼容：CLI 声明可接受的 server 版本范围。
/// 当前协议演进快，只保证同 minor 兼容；升级 CLI 后旧 server 需同步升级。
/// 范围从 Cargo.toml 版本自动生成，无需手动维护。
fn compatible_server_versions(cli_version: &str) -> String {
    let version = cli_version;
    // 开发版本跳过兼容性检查
    if version.contains("-dev") {
        return ">=0.0.0".to_string();
    }
    // 解析当前版本，生成同 minor 范围
    if let Ok(v) = semver::Version::parse(version) {
        format!(">={}, <{}.{}.0", version, v.major, v.minor + 1)
    } else {
        // fallback: 允许任何版本
        ">=0.0.0".to_string()
    }
}

/// §13 版本兼容检查：解析 server 返回的版本号并校验是否在兼容范围内。
/// 不兼容时报错退出（设计决策⑦：ping 时检查，一次性）。
/// 开发版本（-dev 后缀）跳过检查。
fn check_server_version_for(cli_version: &str, server_version: &str) -> Result<()> {
    // 开发版本 CLI 跳过兼容性检查
    if cli_version.contains("-dev") || server_version.contains("-dev") {
        return Ok(());
    }
    let range = compatible_server_versions(cli_version);
    let req = semver::VersionReq::parse(&range)
        .expect("compatible_server_versions must return a valid semver range");
    let parsed = semver::Version::parse(server_version)
        .with_context(|| format!("server returned invalid version {server_version:?}"))?;
    if !req.matches(&parsed) {
        bail!(
            "server version {server_version} incompatible with CLI {cli_version}; \
             please upgrade server (run `llm-proxy serve` on the server machine)"
        );
    }
    Ok(())
}

pub fn check_server_version(server_version: &str) -> Result<()> {
    check_server_version_for(env!("CARGO_PKG_VERSION"), server_version)
}

/// Try to detect a running server for the given config.
///
/// - `Ok(Some(conn))`：Server 存在且版本兼容
/// - `Ok(None)`：Server 未运行（或无法连接）
/// - `Err(e)`：Server 存在但版本不兼容（§13，报错退出）
///
/// 本地模式：PID 文件 + `/admin/ping` 双重确认；
/// 远程/容器模式：CLI 无宿主 PID 文件，直接 ping `server.listen`（只读无鉴权，
/// 只有 llm-proxy 会响应 `/admin/ping`，不会误判其他服务）。
pub async fn detect_server(config_path: &Path) -> Result<Option<ServerConnection>> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    detect_server_at(config_path, &dirs.runtime_dir).await
}

/// 与 [`detect_server`] 同义，但管理通道 socket 从给定的 runtime dir 推导，
/// 而不是恒取全局 `service::socket_path()`。
///
/// 没有这个入口时，委托分支无法在单测里验证——mock 出来的 `ServerConnection`
/// 会指向开发者正在运行的实例 socket，测试会真的向它发写请求。
pub async fn detect_server_at(
    config_path: &Path,
    runtime_dir: &Path,
) -> Result<Option<ServerConnection>> {
    detect_server_with_version_check(config_path, Some(runtime_dir), true).await
}

/// Read the configuration a client is about to replace wholesale, together with
/// the generation it came from.
///
/// With a live server this goes through the management channel so the body and
/// its revision come from **one** read: reading the file and then asking for a
/// revision separately leaves a window where the two describe different
/// generations, which can only yield either a false conflict or a silent
/// overwrite. Without a server the file is the authority and there is no
/// generation to carry — the local write path is serialized by `ConfigLock`.
pub async fn load_config_for_replace(
    config_path: &Path,
    runtime_dir: &Path,
) -> Result<(crate::config::Config, Option<u64>)> {
    if let Some(server) = detect_server_at(config_path, runtime_dir).await? {
        return server
            .config_snapshot()
            .await
            .map(|(cfg, revision)| (cfg, Some(revision)));
    }
    let cfg = crate::config::Config::load(config_path)?;
    Ok((cfg, None))
}

/// Detect a reachable server for a selector-bearing probe without applying the generic
/// CLI/server compatibility gate. The caller must apply the selector-specific version gate.
pub async fn detect_server_for_probe_selection(
    config_path: &Path,
) -> Result<Option<ServerConnection>> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    detect_server_for_probe_selection_at(config_path, &dirs.runtime_dir).await
}

/// 与 [`detect_server_for_probe_selection`] 同义，但管理通道 socket 从给定的 runtime dir 推导，
/// 而不是恒取全局 `service::socket_path()`。
pub async fn detect_server_for_probe_selection_at(
    config_path: &Path,
    runtime_dir: &Path,
) -> Result<Option<ServerConnection>> {
    detect_server_with_version_check(config_path, Some(runtime_dir), false).await
}

async fn detect_server_with_version_check(
    config_path: &Path,
    runtime_dir_override: Option<&std::path::Path>,
    check_version: bool,
) -> Result<Option<ServerConnection>> {
    let cfg = match crate::config::Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(_) => return Ok(None),
    };
    let server = match &cfg.server {
        Some(s) => s,
        None => return Ok(None),
    };
    let listen = &server.listen;
    let base_url = format!("http://{listen}");
    let uds_path = match runtime_dir_override {
        Some(runtime_dir) => crate::service::socket_path_in(runtime_dir),
        None => crate::service::socket_path(),
    };
    let management_port = server.management_port();

    // 全部 /admin/* 端点（含 /admin/ping）迁入 UDS 管理通道，
    // 公开 HTTP listener 上不再有 /admin/ping。检测 server 是否运行也走 UDS。
    let resp = match uds_ping(&uds_path).await {
        Ok(resp) => resp,
        // 连接失败视为"Server 未运行"（独立模式），而非版本错误
        Err(_) => return Ok(None),
    };
    let version = resp
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");
    if check_version {
        check_server_version(version)?;
    }
    let config_identity = resp
        .get("config_identity")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(Some(ServerConnection {
        base_url,
        server_version: Some(version.to_string()),
        config_identity,
        uds_path,
        management_port,
    }))
}

/// 通过 UDS 管理通道发送 GET /admin/ping。
///
/// 独立函数而非 `ServerConnection` 方法，因为 `detect_server` 在创建连接
/// 之前就需要用它来探活。
#[cfg(unix)]
async fn uds_ping(socket_path: &std::path::Path) -> Result<serde_json::Value> {
    if !socket_path.exists() {
        bail!("management socket not found at {}", socket_path.display());
    }
    let client = reqwest::Client::builder()
        .unix_socket(socket_path)
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .context("failed to build UDS ping client")?;
    let resp = client
        .get("http://localhost/admin/ping")
        .send()
        .await
        .context("UDS ping failed")?;
    if !resp.status().is_success() {
        bail!("ping returned {}", resp.status());
    }
    resp.json().await.context("invalid ping response")
}

#[cfg(not(unix))]
async fn uds_ping(_socket_path: &std::path::Path) -> Result<serde_json::Value> {
    bail!("UDS ping not supported on Windows");
}

/// Check if a process is alive.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) checks existence without sending a signal
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

// ── Admin API Client Methods ─────────────────────────────────────────────────

impl ServerConnection {
    /// Check the generic CLI/server compatibility gate for this connection.
    pub fn check_server_compatibility(&self) -> Result<()> {
        check_server_version(self.server_version.as_deref().unwrap_or("0.0.0"))
    }

    /// Check whether this server connection supports probe selection (§4).
    pub fn check_probe_selection_support(&self) -> Result<()> {
        let version = self.server_version.as_deref().unwrap_or("0.0.0");
        check_probe_selection_version_for(env!("CARGO_PKG_VERSION"), version)
    }

    /// GET /admin/ping — via UDS management channel
    pub async fn ping(&self) -> Result<serde_json::Value> {
        self.uds_get("/admin/ping").await
    }

    /// GET /admin/status — via UDS management channel
    pub async fn status(&self) -> Result<serde_json::Value> {
        self.uds_get("/admin/status").await
    }

    /// GET /admin/provider/list — via UDS management channel
    pub async fn provider_list(&self) -> Result<serde_json::Value> {
        self.uds_get("/admin/provider/list").await
    }

    /// GET /admin/provider/{id} — via UDS management channel
    pub async fn provider_info(&self, id: &str) -> Result<serde_json::Value> {
        self.uds_get(&format!("/admin/provider/{}", id)).await
    }

    /// GET /admin/model/list — via UDS management channel
    pub async fn model_list(&self) -> Result<serde_json::Value> {
        self.uds_get("/admin/model/list").await
    }

    /// GET /admin/model/{id} — via UDS management channel
    pub async fn model_info(&self, id: &str) -> Result<serde_json::Value> {
        self.uds_get(&format!("/admin/model/{}", id)).await
    }

    /// GET /admin/client-config/{client} — via UDS management channel
    pub async fn client_config(&self, client: &str) -> Result<serde_json::Value> {
        self.uds_get(&format!("/admin/client-config/{}", client))
            .await
    }

    /// GET /admin/usage — via UDS management channel
    pub async fn usage(
        &self,
        period: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        endpoint: Option<&str>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<serde_json::Value> {
        let mut path = "/admin/usage".to_string();
        let mut params = Vec::new();
        if let Some(p) = period {
            params.push(format!("period={}", p));
        }
        if let Some(p) = provider {
            params.push(format!("provider={}", p));
        }
        if let Some(m) = model {
            params.push(format!("model={}", m));
        }
        if let Some(e) = endpoint {
            params.push(format!("endpoint={}", e));
        }
        if let Some(l) = limit {
            params.push(format!("limit={}", l));
        }
        if let Some(o) = offset {
            params.push(format!("offset={}", o));
        }
        if !params.is_empty() {
            path.push('?');
            path.push_str(&params.join("&"));
        }
        self.uds_get(&path).await
    }

    /// Helper: GET request via UDS management channel
    async fn uds_get(&self, path: &str) -> Result<serde_json::Value> {
        let client = self.build_management_probe_client(None)?;
        let url = format!("http://localhost{}", path);
        let resp = client
            .get(&url)
            .send()
            .await
            .context("failed to reach management channel")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("invalid response")?;
        if status.is_success() {
            Ok(body)
        } else {
            let msg = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("management channel error: {}", msg)
        }
    }

    /// 管理通道探测请求的目标 URL。
    /// Unix：走 UDS，URL 的 host 仅占位（reqwest `unix_socket` 忽略连接目标）。
    /// Windows：localhost TCP 管理通道，端口从配置读取（默认 8989）。
    #[cfg(unix)]
    fn management_probe_url(&self) -> &'static str {
        "http://localhost/admin/status/probe"
    }
    #[cfg(windows)]
    fn management_probe_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/admin/status/probe",
            self.management_port
        )
    }

    /// 构建打到管理通道的探测 client（探测有真实上游副作用，
    /// 与写操作一样只能经 owner-only 管理通道发送，不再走公开 listener）。
    /// `timeout` 为 `None` 时不设全局超时（SSE 流式，慢速探测不能被整体切断）。
    fn build_management_probe_client(&self, timeout: Option<Duration>) -> Result<reqwest::Client> {
        // 控制面探测不得经环境代理转发（spec §11.8）。承重位置要说准：Unix 上
        // `.unix_socket()` 已经决定传输层，环境代理接管不了它——变异测试删掉本行的
        // `.no_proxy()` 后用例仍绿，所以它在 Linux 上是纵深防御而不是承重件；真正会被
        // 环境代理劫持的是 Windows 分支的回环 TCP 目标，那里本行必需，而该分支在本机
        // 不编译也不执行（与端口发现同一缺口）。
        let mut builder = reqwest::Client::builder().no_proxy();
        #[cfg(unix)]
        {
            if !self.uds_path.exists() {
                bail!(
                    "management socket not found at {}: probe must run on the server machine \
                     (remote management is not supported)",
                    self.uds_path.display()
                );
            }
            builder = builder.unix_socket(self.uds_path.clone());
        }
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        builder
            .build()
            .context("failed to build management probe client")
    }

    /// POST /admin/status/probe — request server-managed online probe (§12.5)
    /// 经管理通道发送；使用更长的超时（60 秒），因为 probe 需要探测多个 provider
    pub async fn status_probe(
        &self,
        selection: &crate::status::ProbeSelection,
    ) -> Result<serde_json::Value> {
        if !selection.is_empty() {
            self.check_probe_selection_support()?;
            self.check_server_compatibility()?;
        }
        let client = self.build_management_probe_client(Some(Duration::from_secs(60)))?;
        let mut req = client.post(self.management_probe_url());
        if !selection.is_empty() {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(selection);
        }
        let resp = req
            .send()
            .await
            .context("failed to reach management channel")?;
        let status = resp.status();
        if status.is_success() {
            resp.json().await.context("invalid status probe response")
        } else {
            let text = resp.text().await.unwrap_or_default();
            let msg = if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
                body.get("error")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(text)
            } else if !text.trim().is_empty() {
                text
            } else {
                format!("status {}", status)
            };
            bail!("server error: {msg}")
        }
    }

    /// POST /admin/status/probe — streaming server-managed online probe (§12.5)
    /// 经管理通道发送；UDS 上的 HTTP/1 原生支持 SSE 流式（服务端 management_router 用同一 http1 transport）
    pub async fn status_probe_stream(
        &self,
        selection: &crate::status::ProbeSelection,
    ) -> Result<impl Stream<Item = Result<SseEvent>> + Send + 'static> {
        if !selection.is_empty() {
            self.check_probe_selection_support()?;
            self.check_server_compatibility()?;
        }
        let client = self.build_management_probe_client(None)?;
        let mut req = client
            .post(self.management_probe_url())
            .header("Accept", "text/event-stream");
        if !selection.is_empty() {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(selection);
        }
        let resp = req.send().await.context(
            "failed to reach management channel; is llm-proxy server running and responsive?",
        )?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
                body.get("error")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(text)
            } else if !text.trim().is_empty() {
                text
            } else {
                format!("status {}", status)
            };
            bail!("server error: {msg}");
        }
        Ok(response_to_sse_stream(resp))
    }

    /// POST /admin/model/add — 走 UDS 管理通道
    pub async fn model_add(
        &self,
        model_id: &str,
        context_window: Option<i64>,
        max_output: Option<i64>,
        copy_from: Option<&str>,
        routes: Option<Vec<crate::config::RouteEntry>>,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/model/add",
            &serde_json::json!({
                "model_id": model_id,
                "context_window": context_window,
                "max_output": max_output,
                "copy_from": copy_from,
                "routes": routes,
            }),
        )
        .await
    }

    /// POST /admin/model/set — 走 UDS 管理通道
    pub async fn model_set(
        &self,
        model_id: &str,
        options: &crate::model::SetModelOptions,
    ) -> Result<serde_json::Value> {
        let reasoning_levels = match &options.reasoning_levels {
            Some(crate::model::ReasoningLevelsUpdate::Entries(entries)) => Some(entries.clone()),
            Some(crate::model::ReasoningLevelsUpdate::Names(names)) => Some(
                names
                    .iter()
                    .map(|name| crate::config::ReasoningLevelEntry::new(name.clone(), None))
                    .collect(),
            ),
            None => None,
        };
        self.uds_request(
            "POST",
            "/admin/model/set",
            &serde_json::json!({
                "model_id": model_id,
                "context_window": options.context_window,
                "max_output_tokens": options.max_output_tokens,
                "reasoning_levels": reasoning_levels,
                "thinking_level": options.thinking_level,
                "supports_thinking": options.supports_thinking,
                "force_think": options.force_think,
                "force_no_think": options.force_no_think,
                "default_think": options.default_think,
                "enable_features": options.enable_features,
                "disable_features": options.disable_features,
                "description": options.description,
                "clear_description": options.clear_description,
                "chat_reasoning_off_format": options.chat_reasoning_off_format,
                "clear_chat_reasoning_off_format": options.clear_chat_reasoning_off_format,
                "codex_service_tier": options.codex_service_tier,
                "clear_codex_service_tier": options.clear_codex_service_tier,
                "cooldown": options.cooldown,
                "clear_cooldown": options.clear_cooldown,
            }),
        )
        .await
    }

    /// POST /admin/model/remove — 走 UDS 管理通道
    pub async fn model_remove(&self, model_id: &str, force: bool) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/model/remove",
            &serde_json::json!({ "model_id": model_id, "force": force }),
        )
        .await
    }

    /// POST /admin/model/rename — 走 UDS 管理通道
    pub async fn model_rename(
        &self,
        old_model: &str,
        new_model: &str,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/model/rename",
            &serde_json::json!({ "old_model": old_model, "new_model": new_model }),
        )
        .await
    }

    /// POST /admin/model/provider — 走 UDS 管理通道（action: add/remove/move）
    pub async fn model_provider(
        &self,
        action: &str,
        model_id: &str,
        protocol: &str,
        provider: &str,
        upstream_model: Option<&str>,
        to: Option<usize>,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/model/provider",
            &serde_json::json!({
                "action": action,
                "model_id": model_id,
                "protocol": protocol,
                "provider": provider,
                "upstream_model": upstream_model,
                "to": to,
            }),
        )
        .await
    }

    /// GET /admin/provider/list — provider id 列表（走 UDS 管理通道）
    ///
    /// 与 `provider_list` 对称但返回解析后的 `Vec<String>`：`complete-candidates`
    /// 只需要 id 字符串，不需要完整对象。
    pub async fn list_providers(&self) -> Result<Vec<String>> {
        let body = self.uds_get("/admin/provider/list").await?;
        let providers = body
            .get("data")
            .and_then(|d| d.get("providers"))
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(providers)
    }

    /// POST /admin/cooldown/clear — 走 UDS 管理通道
    pub async fn cooldown_clear(
        &self,
        model: Option<&str>,
        provider: &str,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/cooldown/clear",
            &serde_json::json!({ "provider": provider, "model": model }),
        )
        .await
    }

    /// POST /admin/cooldown/set — 走 UDS 管理通道
    pub async fn cooldown_set(
        &self,
        provider: &str,
        model: &str,
        duration: &str,
        protocol: Option<&str>,
        kind: Option<&str>,
        reason: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/cooldown/set",
            &serde_json::json!({
                "provider": provider,
                "model": model,
                "duration": duration,
                "protocol": protocol,
                "kind": kind,
                "reason": reason,
            }),
        )
        .await
    }

    /// POST /admin/provider/add — 走 UDS 管理通道
    #[allow(clippy::too_many_arguments)]
    pub async fn add_provider(
        &self,
        provider_id: &str,
        provider_name: Option<&str>,
        api_key_env: Option<&str>,
        no_api_key: bool,
        provider_type: Option<&str>,
        endpoint_url: Option<&str>,
        models: Option<&[String]>,
    ) -> Result<serde_json::Value> {
        self.add_provider_with_options(
            provider_id,
            provider_name,
            api_key_env,
            no_api_key,
            provider_type,
            endpoint_url,
            models,
            false,
            None,
        )
        .await
    }

    /// POST /admin/provider/add — 走 UDS 管理通道（扩展版，含 skip_probe 和 pending_account）
    #[allow(clippy::too_many_arguments)]
    pub async fn add_provider_with_options(
        &self,
        provider_id: &str,
        provider_name: Option<&str>,
        api_key_env: Option<&str>,
        no_api_key: bool,
        provider_type: Option<&str>,
        endpoint_url: Option<&str>,
        models: Option<&[String]>,
        skip_probe: bool,
        pending_account: Option<&crate::auth::PendingAccount>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "provider_id": provider_id,
            "provider_name": provider_name,
            "api_key_env": api_key_env,
            "no_api_key": no_api_key,
            "provider_type": provider_type,
            "endpoint_url": endpoint_url,
            "models": models,
            "skip_probe": skip_probe,
        });
        if let Some(pending) = pending_account {
            body["pending_account"] = serde_json::to_value(pending)?;
        }
        self.uds_request("POST", "/admin/provider/add", &body).await
    }

    /// DELETE /admin/provider/remove — 走 UDS 管理通道（北极星：写操作仅 UDS）
    pub async fn remove_provider(&self, id: &str, force: bool) -> Result<serde_json::Value> {
        self.uds_request(
            "DELETE",
            "/admin/provider/remove",
            &serde_json::json!({ "id": id, "force": force }),
        )
        .await
    }

    /// POST /admin/oauth/logout — 登出 OAuth provider（委托
    /// server 删凭据并刷新内存缓存，server 存活时不再由 CLI 直写磁盘）。
    pub async fn oauth_logout(&self, provider_id: &str) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/oauth/logout",
            &serde_json::json!({ "provider_id": provider_id }),
        )
        .await
    }

    // ── Token Management (delegation) ───────────────────────────────

    /// POST /admin/token/create — 创建 token（委托模式）
    pub async fn token_create(
        &self,
        name: &str,
        models: &[String],
        windows: &[crate::token::manage::NewQuotaWindow],
        expires: Option<&str>,
    ) -> Result<serde_json::Value> {
        let windows_json: Vec<serde_json::Value> = windows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "window": w.window,
                    "quota_mode": crate::token::dimension::label(&w.quota_mode),
                    "quota_limit": w.quota_limit,
                })
            })
            .collect();
        self.uds_request(
            "POST",
            "/admin/token/create",
            &serde_json::json!({
                "name": name,
                "models": models,
                "windows": windows_json,
                "expires": expires,
            }),
        )
        .await
    }

    /// POST /admin/token/delete — 删除 token（委托模式）
    pub async fn token_delete(&self, key_or_name: &str) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/token/delete",
            &serde_json::json!({ "key_or_name": key_or_name }),
        )
        .await
    }

    /// POST /admin/token/rename — 重命名 token（委托模式）
    pub async fn token_rename(
        &self,
        key_or_name: &str,
        new_name: &str,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/token/rename",
            &serde_json::json!({
                "key_or_name": key_or_name,
                "new_name": new_name,
            }),
        )
        .await
    }

    /// POST /admin/token/reset-usage — 重置 token 用量（委托模式）
    pub async fn token_reset_usage(&self, key_or_name: &str) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/token/reset-usage",
            &serde_json::json!({ "key_or_name": key_or_name }),
        )
        .await
    }

    /// POST /admin/provider/reselect — 走 UDS 管理通道
    pub async fn reselect_provider(
        &self,
        provider_id: &str,
        models: &[String],
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/provider/reselect",
            &serde_json::json!({
                "provider_id": provider_id,
                "models": models,
            }),
        )
        .await
    }

    /// POST /admin/provider/copy — 走 UDS 管理通道
    pub async fn copy_provider(
        &self,
        source: &str,
        target: &str,
        api_key_env: Option<&str>,
        no_api_key: bool,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/provider/copy",
            &serde_json::json!({
                "source": source,
                "target": target,
                "api_key_env": api_key_env,
                "no_api_key": no_api_key,
            }),
        )
        .await
    }

    /// GET /admin/config/snapshot — 配置正文 + 它所属的代（revision）。
    ///
    /// 整体替换配置前的必读入口：正文与代际必须来自同一次读取，否则 CAS 无意义。
    pub async fn config_snapshot(&self) -> Result<(crate::config::Config, u64)> {
        let resp = self
            .uds_request("GET", "/admin/config/snapshot", &serde_json::json!({}))
            .await?;
        let data = resp
            .get("data")
            .cloned()
            .context("config snapshot response has no data")?;
        let revision = data
            .get("revision")
            .and_then(|v| v.as_u64())
            .context("config snapshot response has no revision")?;
        let config: crate::config::Config = serde_json::from_value(
            data.get("config")
                .cloned()
                .context("config snapshot response has no config")?,
        )
        .context("failed to parse config snapshot")?;
        Ok((config, revision))
    }

    /// POST /admin/config/update — 整体替换配置（TUI 编辑器保存委托）。
    /// `base` 是 `config` 读出时的代；`None` 会被 server 以 `409` 拒绝，
    /// 因为无依据的整体替换正是静默覆盖本身。
    pub async fn config_update(
        &self,
        cfg: &crate::config::Config,
        base: Option<u64>,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/config/update",
            &serde_json::json!({ "base": base, "config": cfg }),
        )
        .await
    }

    /// POST /admin/oauth/write — 写入 OAuth 账号（CLI login 委托 server）
    pub async fn oauth_write(
        &self,
        provider_id: &str,
        oauth_type: &str,
        account: &str,
        account_data: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.uds_request(
            "POST",
            "/admin/oauth/write",
            &oauth_write_request_body(provider_id, oauth_type, account, account_data)?,
        )
        .await
    }

    /// POST /admin/config/reload — 走 UDS 管理通道
    pub async fn reload_config(&self) -> Result<serde_json::Value> {
        self.uds_request("POST", "/admin/config/reload", &serde_json::json!({}))
            .await
    }

    const MANAGEMENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// 写操作经 UDS 管理通道发送（HTTP 1.1 over UnixStream）。
    /// 远程/容器环境无本机 socket → 报错（远程管理不做，须在 server 机器上执行）。
    /// Windows 使用 Named Pipe 作为管理通道。
    ///
    /// 通道地址来自 `self.uds_path`（由 `detect_server*` 按 runtime dir 推导），不是
    /// 全局常量：读路径（`build_management_probe_client`）早已按连接注入，写路径若
    /// 仍硬编码全局 socket，委托分支就无法在不触碰真实实例的前提下被测试覆盖。
    async fn uds_request(
        &self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        #[cfg(unix)]
        {
            if !self.uds_path.exists() {
                bail!(
                    "management socket not found at {}: write operations must run on \
                     the server machine (remote management is not supported)",
                    self.uds_path.display()
                );
            }
        }
        Self::uds_request_impl(method, path, body, &self.uds_path, self.management_port).await
    }

    #[cfg(unix)]
    async fn uds_request_impl(
        method: &str,
        path: &str,
        body: &serde_json::Value,
        sock: &std::path::Path,
        _management_port: u16,
    ) -> Result<serde_json::Value> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(sock)
            .await
            .context("failed to connect management socket")?;
        let body_str = serde_json::to_string(body).context("failed to serialize request body")?;
        let request = format!(
            "{method} {path} HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        stream
            .write_all(request.as_bytes())
            .await
            .context("failed to write management request")?;
        let mut response = String::new();
        tokio::time::timeout(
            Self::MANAGEMENT_REQUEST_TIMEOUT,
            stream.read_to_string(&mut response),
        )
        .await
        .context("management response timed out")?
        .context("failed to read management response")?;
        parse_uds_response(&response)
    }

    #[cfg(windows)]
    async fn uds_request_impl(
        method: &str,
        path: &str,
        body: &serde_json::Value,
        sock: &std::path::Path,
        management_port: u16,
    ) -> Result<serde_json::Value> {
        // Windows: 通过 TCP localhost 连接管理通道，端口从配置读取
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = format!("127.0.0.1:{management_port}");
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .with_context(|| format!("failed to connect management channel at {addr}"))?;
        let body_str = serde_json::to_string(body).context("failed to serialize request body")?;
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        stream
            .write_all(request.as_bytes())
            .await
            .context("failed to write management request")?;
        let mut response = String::new();
        tokio::time::timeout(
            Self::MANAGEMENT_REQUEST_TIMEOUT,
            stream.read_to_string(&mut response),
        )
        .await
        .context("management response timed out")?
        .context("failed to read management response")?;
        parse_uds_response(&response)
    }
}

/// Build an admin resource URL while percent-encoding each path segment.
fn admin_resource_url(base_url: &str, segments: &[&str]) -> Result<url::Url> {
    let mut url = url::Url::parse(base_url).context("invalid admin base URL")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("admin base URL cannot be a base"))?;
        path.extend(segments);
    }
    Ok(url)
}

/// Build the flattened payload expected by the server's OAuthWriteRequest.
fn oauth_write_request_body(
    provider_id: &str,
    oauth_type: &str,
    account: &str,
    account_data: &serde_json::Value,
) -> Result<serde_json::Value> {
    let mut body = account_data
        .as_object()
        .cloned()
        .context("OAuth account data must be a JSON object")?;
    body.insert(
        "provider_id".to_string(),
        serde_json::Value::String(provider_id.to_string()),
    );
    body.insert(
        "oauth_type".to_string(),
        serde_json::Value::String(oauth_type.to_string()),
    );
    body.insert(
        "account".to_string(),
        serde_json::Value::String(account.to_string()),
    );
    Ok(serde_json::Value::Object(body))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminClientError {
    pub status: u16,
    pub message: String,
    pub kind: Option<String>,
}

impl std::fmt::Display for AdminClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server error: {}", self.message)
    }
}

impl std::error::Error for AdminClientError {}

/// 是否为**探测失败**（`ProbeFailed`）——与 `is_recoverable_warning` 的区别是
/// 不含 `AuthNotReady`：凭据缺失不是"模型连不上"，不应走"放行写配置"分支。
pub fn is_probe_failure(err: &anyhow::Error) -> bool {
    if let Some(pae) = err.downcast_ref::<crate::connect::ProviderAddError>() {
        return pae.kind == crate::connect::ProviderAddErrorKind::ProbeFailed;
    }
    if let Some(ace) = err.downcast_ref::<AdminClientError>() {
        return ace.kind.as_deref() == Some("probe_failed");
    }
    false
}

pub fn is_recoverable_warning(err: &anyhow::Error) -> bool {
    if let Some(pae) = err.downcast_ref::<crate::connect::ProviderAddError>() {
        return matches!(
            pae.kind,
            crate::connect::ProviderAddErrorKind::ProbeFailed
                | crate::connect::ProviderAddErrorKind::AuthNotReady
        );
    }
    if let Some(ace) = err.downcast_ref::<AdminClientError>() {
        return matches!(
            ace.kind.as_deref(),
            Some("probe_failed") | Some("auth_not_ready")
        );
    }
    false
}

/// 解析 UDS 管理通道的 HTTP 响应：状态码 + JSON body。
fn parse_uds_response(response: &str) -> Result<serde_json::Value> {
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    let value: serde_json::Value =
        serde_json::from_str(body).context("invalid management response body")?;
    if (200..300).contains(&status) {
        Ok(value)
    } else {
        let msg = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        Err(AdminClientError {
            status,
            message: msg.to_string(),
            kind,
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_write_request_body_flattens_account_data() {
        let account_data = serde_json::json!({
            "account_label": "redacted@example.com",
            "project_id": "project-id",
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "expires_at_unix": 2_000_000_000i64,
            "updated_at_unix": 1_900_000_000i64,
        });
        let body =
            oauth_write_request_body("some-oauth-provider", "openai", "account-1", &account_data)
                .expect("object account data");

        assert_eq!(body["provider_id"], "some-oauth-provider");
        assert_eq!(body["oauth_type"], "openai");
        assert_eq!(body["account"], "account-1");
        assert_eq!(body["account_label"], "redacted@example.com");
        assert!(body.get("account_data").is_none());
    }

    #[test]
    fn oauth_write_request_body_matches_server_request_schema() {
        let body = oauth_write_request_body(
            "some-oauth-provider",
            "openai",
            "account-1",
            &serde_json::json!({
                "account_label": "redacted@example.com",
                "project_id": "project-id",
                "access_token": "access-token",
                "refresh_token": "refresh-token",
                "expires_at_unix": 2_000_000_000i64,
                "updated_at_unix": 1_900_000_000i64,
            }),
        )
        .expect("object account data");
        let request: crate::admin::OAuthWriteRequest =
            serde_json::from_value(body).expect("server request schema");

        assert_eq!(request.provider_id, "some-oauth-provider");
        assert_eq!(request.oauth_type, "openai");
        assert_eq!(request.account, "account-1");
        assert_eq!(
            request.account_data["account_label"],
            "redacted@example.com"
        );
        assert!(request.account_data.get("account_data").is_none());
    }

    #[test]
    fn admin_resource_url_encodes_path_segments() {
        let url = admin_resource_url("http://127.0.0.1:8989", &["admin", "model", "model/a b"])
            .expect("valid admin URL");
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8989/admin/model/model%2Fa%20b"
        );
    }

    #[test]
    fn oauth_write_request_body_rejects_non_object_account_data() {
        let error = oauth_write_request_body(
            "provider",
            "openai",
            "account",
            &serde_json::json!("not-an-account"),
        )
        .expect_err("non-object account data must be rejected");
        assert!(error.to_string().contains("JSON object"));
    }

    #[test]
    fn server_version_within_compatible_range_is_accepted() {
        // §13.3：0.2.x 均在兼容范围内
        assert!(check_server_version_for("0.2.0", "0.2.0").is_ok());
        assert!(check_server_version_for("0.2.0", "0.2.7").is_ok());
        assert!(check_server_version_for("0.2.0", "0.2.99").is_ok());
    }

    #[test]
    fn server_version_outside_compatible_range_is_rejected() {
        // §13.5：不兼容时报错退出
        let err = check_server_version_for("0.2.0", "0.1.5").expect_err("0.1.x should be rejected");
        assert!(err.to_string().contains("incompatible"));
        assert!(err.to_string().contains("upgrade server"));
        assert!(check_server_version_for("0.2.0", "0.3.0").is_err());
        assert!(check_server_version_for("0.2.0", "1.0.0").is_err());
    }

    #[test]
    fn development_cli_accepts_any_stable_server_version() {
        assert!(check_server_version_for("0.0.0-dev", "0.1.5").is_ok());
        assert!(check_server_version_for("0.0.0-dev", "1.0.0").is_ok());
    }

    #[test]
    fn malformed_server_version_is_rejected() {
        let err =
            check_server_version_for("0.2.0", "not-a-version").expect_err("malformed version");
        assert!(err.to_string().contains("invalid version"));
    }

    /// 起一个 mock admin server，返回指定 version 的 /admin/ping。
    /// 改为 UDS server（/admin/ping 已迁入管理通道）。
    #[cfg(unix)]
    async fn spawn_mock_admin_server(
        version: &'static str,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let app = axum::Router::new().route(
            "/admin/ping",
            axum::routing::get(move || async move {
                axum::Json(serde_json::json!({ "status": "ok", "version": version }))
            }),
        );
        spawn_mock_uds_admin_server(app).await
    }

    fn write_temp_config(listen: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, format!("[server]\nlisten = \"{listen}\"\n"))
            .expect("write config");
        (temp, config_path)
    }

    /// Spawn a UDS mock admin server at the canonical `llm-proxy.sock` path
    /// inside the given runtime dir, so `detect_server_at(config, runtime_dir)`
    /// can find it. Returns the config dir (also serves as runtime dir) and
    /// the config path.
    #[cfg(unix)]
    async fn spawn_mock_uds_at_runtime_dir(
        app: axum::Router,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        use tokio::net::UnixListener;
        let dir = tempfile::Builder::new()
            .prefix("llm-proxy-runtime-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let sock = dir.path().join("llm-proxy.sock");
        let listener = UnixListener::bind(&sock).expect("bind uds mock");
        let app_clone = app.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let service = hyper_util::service::TowerToHyperService::new(
                    app_clone.clone().into_service::<hyper::body::Incoming>(),
                );
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").expect("write config");
        (dir, config_path)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn detect_server_accepts_running_compatible_server() {
        // §13：运行中的 0.2.x server → Ok(Some)
        // detect_server 改走 UDS 探活
        let app = axum::Router::new().route(
            "/admin/ping",
            axum::routing::get(|| async move {
                axum::Json(
                    serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }),
                )
            }),
        );
        let (dir, config_path) = spawn_mock_uds_at_runtime_dir(app).await;
        let runtime_dir = dir.path();
        let conn = detect_server_at(&config_path, runtime_dir)
            .await
            .expect("detect");
        assert!(conn.is_some(), "compatible server should be detected");
        let ping = conn.unwrap().ping().await.expect("ping");
        assert_eq!(ping["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn detect_server_accepts_old_version_in_development_build() {
        // 开发版 CLI 跳过版本检查，便于本地 server/client 联调。
        // 正式 SemVer 构建不属于 dev 通道，拒绝分支由纯函数版本测试覆盖。
        if !crate::version::CLI_VERSION.starts_with("0.0.0-dev") {
            return;
        }
        let app = axum::Router::new().route(
            "/admin/ping",
            axum::routing::get(|| async move {
                axum::Json(serde_json::json!({ "status": "ok", "version": "0.1.0" }))
            }),
        );
        let (dir, config_path) = spawn_mock_uds_at_runtime_dir(app).await;
        let runtime_dir = dir.path();
        let conn = detect_server_at(&config_path, runtime_dir)
            .await
            .expect("development builds accept older server versions");
        assert!(conn.is_some());
    }

    #[tokio::test]
    async fn detect_server_returns_none_when_unreachable() {
        // 独立模式：无 server 在监听 → Ok(None)
        // UDS socket 不存在 → Ok(None)
        let (dir, config_path) = write_temp_config("127.0.0.1:0");
        let runtime_dir = dir.path();
        let conn = detect_server_at(&config_path, runtime_dir)
            .await
            .expect("detect");
        assert!(conn.is_none());
    }

    /// the management-channel probe targets loopback and must never
    /// route through an environment proxy.
    ///
    /// `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` are process-global, and reqwest
    /// reads them when a client is built. Poisoning them in-process would
    /// hijack every other test that builds a client to 127.0.0.1 and runs
    /// concurrently — `#[serial]` does not help because un-annotated tests
    /// still run in parallel with it. So the check re-executes this test in a
    /// child process of the same test binary, where the poisoning is confined.
    #[tokio::test]
    async fn detect_server_ignores_http_proxy_env_var() {
        const CHILD_ENV: &str = "LLM_PROXY_TEST_PROXY_ISOLATION_CHILD";

        if std::env::var_os(CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("resolve current test executable");
            let output = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "admin_client::tests::detect_server_ignores_http_proxy_env_var",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("HTTPS_PROXY", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1")
                // Make the check strict: nothing may exempt loopback for us.
                .env("NO_PROXY", "")
                .env_remove("no_proxy")
                .output()
                .expect("spawn proxy-isolation child");

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A failing child reports "0 passed; 1 failed" and a non-zero exit,
            // so check the exit status first: otherwise the drift guard below
            // would mask the real failure with a misleading message.
            if !output.status.success() {
                panic!(
                    "isolated child failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    output.status
                );
            }
            // Guard against a renamed test making the filter match nothing,
            // which would exit 0 without ever running the body.
            assert!(
                stdout.contains("1 passed") && stdout.contains("0 failed"),
                "isolated child ran no test (filter drift?)\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            return;
        }

        // Child body: every proxy env var points at a dead port, yet the UDS ping
        // must still reach the local mock admin server (Unix socket transport
        // is immune to HTTP proxy env vars).
        let app = axum::Router::new().route(
            "/admin/ping",
            axum::routing::get(|| async move {
                axum::Json(
                    serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }),
                )
            }),
        );
        let (dir, config_path) = spawn_mock_uds_at_runtime_dir(app).await;
        let runtime_dir = dir.path();
        let conn = detect_server_at(&config_path, runtime_dir)
            .await
            .expect("detect must not fail when a dead proxy is configured");
        assert!(
            conn.is_some(),
            "detect_server must reach the UDS socket despite HTTP_PROXY pointing at a dead port"
        );
    }

    /// `status_probe` / `status_probe_stream` 也走管理
    /// 通道，其 client 由 `build_management_probe_client` 构造，必须与
    /// `detect_server` 一样不受环境代理影响。代理变量是进程全局的，`#[serial]` 挡不
    /// 住未标注的并发用例，因此沿用子进程自隔离手法（见上一条测试的注释）。
    #[tokio::test]
    async fn management_probe_ignores_http_proxy_env_var() {
        const CHILD_ENV: &str = "LLM_PROXY_TEST_MGMT_PROXY_ISOLATION_CHILD";

        if std::env::var_os(CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("resolve current test executable");
            let output = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "admin_client::tests::management_probe_ignores_http_proxy_env_var",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("HTTPS_PROXY", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1")
                .env("NO_PROXY", "")
                .env_remove("no_proxy")
                .output()
                .expect("spawn management-probe proxy-isolation child");

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                panic!(
                    "isolated child failed (status {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    output.status
                );
            }
            assert!(
                stdout.contains("1 passed") && stdout.contains("0 failed"),
                "isolated child ran no test (filter drift?)\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            return;
        }

        // Child body: the probe goes over a real UDS listener while every proxy
        // env var points at a dead port. With `.no_proxy()` gone, reqwest hands
        // the request to that port and the probe fails despite a healthy channel.
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/admin/status/probe",
            post(|| async {
                axum::Json(serde_json::json!({
                    "results": [{ "name": "deepseek", "online": true, "latency_ms": 12 }],
                }))
            }),
        );
        let (_temp, sock) = spawn_mock_uds_admin_server(app).await;
        let conn = ServerConnection {
            base_url: "http://unused-public-listener".to_string(),
            server_version: None,
            config_identity: None,
            uds_path: sock,
            management_port: 8989,
        };
        let sel = crate::status::ProbeSelection {
            provider: None,
            model: None,
        };
        let value = conn
            .status_probe(&sel)
            .await
            .expect("probe must reach the local management socket despite HTTP_PROXY");
        assert!(
            value.to_string().contains("deepseek"),
            "the probe response must come from the UDS server, got: {value}"
        );
    }

    /// Spawn a UDS mock admin server that serves the given routes (path → JSON body).
    /// Returns the temp dir (caller must keep alive) and the socket path.
    #[cfg(unix)]
    async fn spawn_mock_uds_admin_server_with_routes(
        routes: Vec<(&'static str, serde_json::Value)>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut app = axum::Router::new();
        for (path, body) in routes {
            use axum::routing::get;
            app = app.route(
                path,
                get(move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }),
            );
        }
        spawn_mock_uds_admin_server(app).await
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn list_providers_returns_provider_ids() {
        let (_temp, sock) = spawn_mock_uds_admin_server_with_routes(vec![(
            "/admin/provider/list",
            serde_json::json!({
                "status": "ok",
                "data": { "providers": ["alpha", "beta"] },
                "error": null,
            }),
        )])
        .await;
        let conn = ServerConnection {
            base_url: "http://unused".to_string(),
            server_version: None,
            config_identity: None,
            uds_path: sock,
            management_port: 8989,
        };
        let ids = conn.list_providers().await.expect("list providers");
        assert_eq!(ids, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn list_providers_propagates_server_error() {
        let (_temp, sock) = spawn_mock_uds_admin_server_with_routes(vec![(
            "/admin/provider/list",
            serde_json::json!({
                "status": "error",
                "data": null,
                "error": "boom",
            }),
        )])
        .await;
        let conn = ServerConnection {
            base_url: "http://unused".to_string(),
            server_version: None,
            config_identity: None,
            uds_path: sock,
            management_port: 8989,
        };
        let ids = conn.list_providers().await.expect("no bail on 200");
        assert!(ids.is_empty());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn list_providers_returns_empty_when_data_missing() {
        let (_temp, sock) = spawn_mock_uds_admin_server_with_routes(vec![(
            "/admin/provider/list",
            serde_json::json!({ "status": "ok", "data": null }),
        )])
        .await;
        let conn = ServerConnection {
            base_url: "http://unused".to_string(),
            server_version: None,
            config_identity: None,
            uds_path: sock,
            management_port: 8989,
        };
        let ids = conn.list_providers().await.expect("defensive parse");
        assert!(ids.is_empty());
    }

    /// Serve `app` over a Unix domain socket in a temp dir, mirroring the
    /// management channel transport in `service.rs` (hyper http1). Returns the
    /// temp dir (caller must keep it alive) and the socket path so a
    /// `ServerConnection.uds_path` can point at the mock.
    #[cfg(unix)]
    async fn spawn_mock_uds_admin_server(
        app: axum::Router,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        use tokio::net::UnixListener;
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-mock-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let sock = temp.path().join("mgmt.sock");
        let listener = UnixListener::bind(&sock).expect("bind uds mock");
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let service = hyper_util::service::TowerToHyperService::new(
                    app.clone().into_service::<hyper::body::Incoming>(),
                );
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        (temp, sock)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn status_probe_stream_uses_probe_timeout_not_ping_client_timeout() {
        use axum::response::sse::{Event, Sse};
        use axum::routing::post;
        use std::convert::Infallible;

        let app = axum::Router::new().route(
            "/admin/status/probe",
            post(|| async {
                let stream = futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok::<_, Infallible>(Event::default().event("done").data("{}"))
                });
                Sse::new(stream)
            }),
        );
        let (_temp, sock) = spawn_mock_uds_admin_server(app).await;

        let conn = ServerConnection {
            base_url: "http://unused-public-listener".to_string(),
            server_version: None,
            config_identity: None,
            uds_path: sock.clone(),
            management_port: 8989,
        };
        let stream = conn
            .status_probe_stream(&crate::status::ProbeSelection::default())
            .await
            .expect("probe stream must not inherit ping timeout");
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().expect("event").event, "done");
    }

    #[test]
    fn probe_selection_version_gate_accepts_and_rejects() {
        // Only a dev CLI paired with a dev server may use the dev-build exception.
        assert!(check_probe_selection_version_for("0.0.0-dev", "0.0.0-dev").is_ok());
        assert!(check_probe_selection_version_for("0.0.0-dev", "0.1.0-dev").is_ok());
        let err = check_probe_selection_version_for("0.3.0", "0.1.0-dev").unwrap_err();
        assert!(
            err.to_string()
                .contains("server does not support probe selection")
        );

        // Formal server: < PROBE_SELECTION_MIN_VERSION rejected.
        let err = check_probe_selection_version_for("0.3.0", "0.2.0").unwrap_err();
        assert!(
            err.to_string()
                .contains("server does not support probe selection")
        );

        let err = check_probe_selection_version_for("0.3.0", "0.2.9").unwrap_err();
        assert!(
            err.to_string()
                .contains("server does not support probe selection")
        );

        // Formal server: >= PROBE_SELECTION_MIN_VERSION accepted.
        assert!(check_probe_selection_version_for("0.3.0", "0.3.0").is_ok());
        assert!(check_probe_selection_version_for("0.3.0", "0.3.1").is_ok());
        assert!(check_probe_selection_version_for("0.3.0", "1.0.0").is_ok());
    }

    #[tokio::test]
    async fn probe_selection_version_gate_rejects_without_sending_request() {
        // Point to an invalid address; if a request were attempted, it would fail with network error.
        // With an incompatible version and non-empty selection, it must fail fast with the version gate error.
        let conn = ServerConnection {
            base_url: "http://127.0.0.1:1".to_string(),
            server_version: Some("0.2.0".to_string()),
            config_identity: None,
            uds_path: std::path::PathBuf::from("/tmp/unused-mgmt.sock"),
            management_port: 8989,
        };
        let sel = crate::status::ProbeSelection {
            provider: Some("deepseek".to_string()),
            model: None,
        };
        let err = conn.status_probe(&sel).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("server does not support probe selection")
        );

        let err_stream = match conn.status_probe_stream(&sel).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(
            err_stream
                .to_string()
                .contains("server does not support probe selection")
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn status_probe_and_stream_propagate_4xx_error_body() {
        use axum::http::StatusCode;
        use axum::routing::post;

        let app = axum::Router::new().route(
            "/admin/status/probe",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "status": "error",
                        "error": "unknown provider: 'nonexistent'",
                    })),
                )
            }),
        );
        let (_temp, sock) = spawn_mock_uds_admin_server(app).await;

        let conn = ServerConnection {
            base_url: "http://unused-public-listener".to_string(),
            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            config_identity: None,
            uds_path: sock.clone(),
            management_port: 8989,
        };
        let sel = crate::status::ProbeSelection {
            provider: None,
            model: None,
        };

        // 1. status_probe propagates error body
        let err = conn.status_probe(&sel).await.unwrap_err();
        assert!(
            err.to_string().contains("unknown provider: 'nonexistent'"),
            "got: {err}"
        );

        // 2. status_probe_stream propagates error body
        let err_stream = match conn.status_probe_stream(&sel).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(
            err_stream
                .to_string()
                .contains("unknown provider: 'nonexistent'"),
            "got: {err_stream}"
        );
    }

    #[test]
    fn test_is_recoverable_warning_classification_matrix() {
        use crate::connect::ProviderAddError;

        // 1. ProviderAddError kinds
        let err_probe: anyhow::Error = ProviderAddError::probe_failed("probe failed").into();
        assert!(is_recoverable_warning(&err_probe));

        let err_auth: anyhow::Error = ProviderAddError::auth_not_ready("auth not ready").into();
        assert!(is_recoverable_warning(&err_auth));

        let err_conflict: anyhow::Error = ProviderAddError::config_conflict("conflict").into();
        assert!(!is_recoverable_warning(&err_conflict));

        let err_invalid: anyhow::Error = ProviderAddError::invalid_config("invalid").into();
        assert!(!is_recoverable_warning(&err_invalid));

        let err_internal: anyhow::Error = ProviderAddError::internal_error("internal").into();
        assert!(!is_recoverable_warning(&err_internal));

        // 2. AdminClientError kinds
        let ace_probe: anyhow::Error = AdminClientError {
            status: 422,
            message: "probe failed".to_string(),
            kind: Some("probe_failed".to_string()),
        }
        .into();
        assert!(is_recoverable_warning(&ace_probe));

        let ace_auth: anyhow::Error = AdminClientError {
            status: 422,
            message: "auth not ready".to_string(),
            kind: Some("auth_not_ready".to_string()),
        }
        .into();
        assert!(is_recoverable_warning(&ace_auth));

        let ace_conflict: anyhow::Error = AdminClientError {
            status: 409,
            message: "conflict".to_string(),
            kind: Some("config_conflict".to_string()),
        }
        .into();
        assert!(!is_recoverable_warning(&ace_conflict));

        let ace_invalid: anyhow::Error = AdminClientError {
            status: 400,
            message: "invalid".to_string(),
            kind: Some("invalid_config".to_string()),
        }
        .into();
        assert!(!is_recoverable_warning(&ace_invalid));

        let ace_no_kind: anyhow::Error = AdminClientError {
            status: 500,
            message: "unknown".to_string(),
            kind: None,
        }
        .into();
        assert!(!is_recoverable_warning(&ace_no_kind));

        // 3. Other generic errors
        let generic_err: anyhow::Error = anyhow::anyhow!("uds connection broken");
        assert!(!is_recoverable_warning(&generic_err));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn model_provider_set_action_sends_correct_wire_format() {
        use axum::Json;
        use axum::http::StatusCode;
        use axum::routing::post;

        let app = axum::Router::new().route(
            "/admin/model/provider",
            post(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["action"], "set");
                assert_eq!(body["model_id"], "m1");
                assert_eq!(body["protocol"], "openai-chat");
                assert_eq!(body["provider"], "p1");
                assert_eq!(body["upstream_model"], "new-upstream");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "status": "ok",
                        "data": { "message": "updated provider p1 upstream model" }
                    })),
                )
            }),
        );
        let (_temp, sock) = spawn_mock_uds_admin_server(app).await;

        let conn = ServerConnection {
            base_url: "http://unused".to_string(),
            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            config_identity: None,
            uds_path: sock,
            management_port: 8989,
        };

        let res = conn
            .model_provider("set", "m1", "openai-chat", "p1", Some("new-upstream"), None)
            .await
            .expect("model_provider set request");
        assert_eq!(res["data"]["message"], "updated provider p1 upstream model");
    }
}
