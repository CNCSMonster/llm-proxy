use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::State;
use serde_json::{Value, json};

use crate::config::Config;

pub fn start_background(config_path: &Path, cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let config_path = absolute_config_path(config_path);
    let runtime_dir = state_dir();
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("failed to create runtime dir {}", runtime_dir.display()))?;
    refuse_if_running()?;

    let pid_path = pid_path();
    let log_path = log_path();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log {}", log_path.display()))?;
    let log_for_stderr = log
        .try_clone()
        .with_context(|| format!("failed to clone log {}", log_path.display()))?;

    let mut command = Command::new(exe);
    command
        .arg("--config")
        .arg(&config_path)
        .arg("serve")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr));
    detach(&mut command);

    let mut child = command
        .spawn()
        .context("failed to start llm-proxy in background")?;

    thread::sleep(Duration::from_millis(250));
    if let Some(status) = child
        .try_wait()
        .context("failed to inspect background process")?
    {
        bail!(
            "llm-proxy background process exited early with {status}; see {}",
            log_path.display()
        );
    }

    write_pid(child.id())?;

    println!("llm-proxy started in background");
    println!("  listen: {}", cfg.server.listen);
    println!("  pid: {}", child.id());
    println!("  pid_file: {}", pid_path.display());
    println!("  socket: {}", socket_path().display());
    println!("  log: {}", log_path.display());
    println!("Use `llm-proxy serve --foreground` to run in the foreground.");
    Ok(())
}

pub async fn run_foreground(cfg: Config, config_path: &Path) -> Result<()> {
    fs::create_dir_all(state_dir()).context("failed to create state dir")?;
    refuse_if_running()?;
    // §15.2：server 生命周期持有所有权锁（CLI 持有则等待 10s，server 持有则失败）
    let _ownership = acquire_server_ownership()?;
    write_pid(std::process::id())?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let state = crate::proxy::AppState::new(cfg, config_path.to_path_buf());
    let management = tokio::spawn(run_management_socket(state.clone(), shutdown_tx));
    let result = crate::proxy::serve_state_with_shutdown(state, shutdown_rx).await;
    management.abort();
    let _ = fs::remove_file(pid_path());
    let _ = fs::remove_file(socket_path());
    result
}

/// 获取 server 所有权锁（§15.2）：
/// - CLI 持有 → 等待（默认 10s，CLI 写操作毫秒级）
/// - server 持有 → 立即失败（防多实例）
/// - 成功 → 返回持有者（调用方持有到生命周期结束，drop 释放）
fn acquire_server_ownership() -> Result<crate::ownership::OwnershipLock> {
    acquire_server_ownership_in(&state_dir(), Duration::from_secs(10))
}

/// 在指定 state_dir 上获取 server 所有权锁（可测试性：传入临时目录，
/// 避免修改全局 `LLM_PROXY_STATE_DIR` 环境变量污染并行测试）。
fn acquire_server_ownership_in(
    state_dir: &Path,
    timeout: Duration,
) -> Result<crate::ownership::OwnershipLock> {
    let deadline = Instant::now() + timeout;
    loop {
        match crate::ownership::OwnershipLock::try_acquire(state_dir, "server", "llm-proxy serve") {
            Ok(lock) => return Ok(lock),
            Err(crate::ownership::AcquireError::HeldByCli { metadata }) => {
                if Instant::now() >= deadline {
                    let pid = metadata.as_ref().map(|m| m.pid);
                    bail!(
                        "所有权锁被 CLI 持有（pid={:?}），等待 {}s 超时；请确认该 CLI 进程已退出或完成写操作",
                        pid,
                        timeout.as_secs()
                    );
                }
                eprintln!(
                    "等待 CLI 写操作完成（pid={}）...",
                    metadata
                        .as_ref()
                        .map(|m| m.pid.to_string())
                        .unwrap_or_else(|| "?".into())
                );
                thread::sleep(Duration::from_millis(200));
            }
            Err(crate::ownership::AcquireError::HeldByServer { .. }) => {
                bail!("另一个 llm-proxy server 已持有所有权锁；请确认没有其他 server 实例在运行");
            }
            Err(e) => bail!("所有权锁获取失败: {e}"),
        }
    }
}

pub fn shutdown_background(_config_path: &Path) -> Result<()> {
    if send_management_shutdown().is_ok() {
        if let Some(pid) = running_pid() {
            wait_for_exit(pid, Duration::from_secs(30));
        }
        let _ = fs::remove_file(pid_path());
        let _ = fs::remove_file(socket_path());
        println!("llm-proxy stopped");
        return Ok(());
    }
    let pid_path = pid_path();
    let pid_text = match fs::read_to_string(&pid_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("llm-proxy is not running");
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", pid_path.display()));
        }
    };
    let pid: u32 = pid_text.trim().parse().context("invalid pid file")?;
    terminate_process(pid)?;
    wait_for_exit(pid, Duration::from_secs(5));
    let _ = fs::remove_file(&pid_path);
    println!("llm-proxy stopped");
    println!("  pid: {pid}");
    println!("  pid_file: {}", pid_path.display());
    Ok(())
}

pub fn restart_background(config_path: &Path, cfg: &Config) -> Result<()> {
    shutdown_background(config_path)?;
    start_background(config_path, cfg)
}

/// State directory for runtime files (pid, socket, log, cooldowns, status-cache).
/// 支持 `LLM_PROXY_STATE_DIR` 环境变量覆盖，便于测试和隔离场景。
pub fn state_dir() -> PathBuf {
    std::env::var("LLM_PROXY_STATE_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(dirs::state_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("llm-proxy")
}

pub fn pid_path() -> PathBuf {
    state_dir().join("llm-proxy.pid")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("llm-proxy.sock")
}

pub fn log_path() -> PathBuf {
    state_dir().join("llm-proxy.log")
}

pub fn running_pid() -> Option<u32> {
    let pid = fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    process_exists(pid).then_some(pid)
}

fn refuse_if_running() -> Result<()> {
    let path = pid_path();
    let Some(text) = fs::read_to_string(&path).ok() else {
        return Ok(());
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        let _ = fs::remove_file(&path);
        return Ok(());
    };
    if process_exists(pid) {
        bail!(
            "llm-proxy service is already running (pid {pid}); use `llm-proxy restart` to switch config"
        );
    }
    let _ = fs::remove_file(path);
    Ok(())
}

fn write_pid(pid: u32) -> Result<()> {
    fs::write(pid_path(), format!("{pid}\n"))
        .with_context(|| format!("failed to write pid file {}", pid_path().display()))
}

fn wait_for_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
async fn run_management_socket(
    state: crate::proxy::AppState,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<()> {
    use tokio::net::UnixListener;

    let path = socket_path();
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind management socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let router = management_router(state, shutdown_tx);
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("management accept failed")?;
        let service = hyper_util::service::TowerToHyperService::new(
            router.clone().into_service::<hyper::body::Incoming>(),
        );
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(windows)]
async fn run_management_socket(
    state: crate::proxy::AppState,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<()> {
    // Windows: 管理通道通过 TCP localhost 提供（与 UDS 等价）
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind management TCP listener")?;
    let addr = listener.local_addr().context("failed to get management addr")?;
    tracing::info!("management channel listening on {addr}");
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let router = management_router(state, shutdown_tx);
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("management accept failed")?;
        let service = hyper_util::service::TowerToHyperService::new(
            router.clone().into_service::<hyper::body::Incoming>(),
        );
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// UDS 管理通道 state：写操作（provider/model 增删改、OAuth、shutdown）仅本机管理通道。
#[derive(Clone)]
pub struct ManagementState {
    pub app: crate::proxy::AppState,
    pub shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl axum::extract::FromRef<ManagementState> for crate::proxy::AppState {
    fn from_ref(s: &ManagementState) -> Self {
        s.app.clone()
    }
}

/// 构建管理通道 router：shutdown/state/env + 全部 admin 写端点。
/// 写端点从 TCP 公开接口迁到这里（北极星：写操作仅通过管理通道）。
pub(crate) fn management_router(
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> axum::Router {
    use axum::routing::{delete, get, post};
    let mgmt = ManagementState {
        app: state,
        shutdown_tx,
    };
    axum::Router::new()
        .route("/shutdown", post(shutdown_handler))
        .route("/state", get(state_handler))
        .route("/env", post(env_handler))
        .route("/admin/provider/add", post(crate::admin::provider_add))
        .route(
            "/admin/provider/remove",
            delete(crate::admin::provider_remove),
        )
        .route("/admin/provider/copy", post(crate::admin::provider_copy))
        .route("/admin/config/reload", post(crate::admin::config_reload))
        .route("/admin/config/update", post(crate::admin::config_update))
        .route("/admin/model/add", post(crate::admin::model_add))
        .route("/admin/model/set", post(crate::admin::model_set))
        .route("/admin/model/remove", post(crate::admin::model_remove))
        .route("/admin/model/provider", post(crate::admin::model_provider))
        .route("/admin/cooldown/clear", post(crate::admin::cooldown_clear))
        .route("/admin/oauth/write", post(crate::admin::oauth_write))
        .with_state(mgmt)
}

async fn shutdown_handler(State(mgmt): State<ManagementState>) -> axum::Json<Value> {
    if let Ok(mut tx) = mgmt.shutdown_tx.lock()
        && let Some(tx) = tx.take()
    {
        let _ = tx.send(());
    }
    axum::Json(json!({"ok": true, "shutdown": "scheduled"}))
}

async fn state_handler(State(mgmt): State<ManagementState>) -> axum::Json<Value> {
    axum::Json(mgmt.app.management_state_json())
}

async fn env_handler(
    State(_mgmt): State<ManagementState>,
    axum::Json(body): axum::Json<Value>,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    match apply_env_body_value(&body) {
        Ok(applied) => (
            axum::http::StatusCode::OK,
            axum::Json(json!({"applied": applied})),
        ),
        Err(err) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": err.to_string()})),
        ),
    }
}

fn apply_env_body_value(body: &Value) -> Result<Vec<String>> {
    let env = body
        .get("env")
        .and_then(Value::as_object)
        .context("missing env object")?;
    let mut applied = Vec::new();
    for (key, value) in env {
        let Some(value) = value.as_str() else {
            bail!("env value for {key} must be a string");
        };
        unsafe { std::env::set_var(key, value) };
        applied.push(key.clone());
    }
    Ok(applied)
}

pub fn management_state() -> Result<Value> {
    let response = send_management_request("GET /state HTTP/1.1\r\nconnection: close\r\n\r\n")?;
    parse_http_json_response(&response)
}

pub fn inject_env(env: &std::collections::BTreeMap<String, String>) -> Result<Vec<String>> {
    if env.is_empty() {
        return Ok(Vec::new());
    }
    let body = serde_json::to_string(&json!({ "env": env }))
        .context("failed to serialize env injection request")?;
    let request = format!(
        "POST /env HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let response = send_management_request(&request)?;
    let value = parse_http_json_response(&response)?;
    Ok(value
        .get("applied")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect())
}

#[cfg(test)]
fn build_env_injection_request(env: &std::collections::BTreeMap<String, String>) -> Result<String> {
    let body = serde_json::to_string(&json!({ "env": env }))
        .context("failed to serialize env injection request")?;
    Ok(format!(
        "POST /env HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    ))
}

#[cfg(unix)]
fn send_management_shutdown() -> Result<()> {
    let response = send_management_request(
        "POST /shutdown HTTP/1.1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )?;
    if response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        bail!("management shutdown failed")
    }
}

#[cfg(unix)]
fn send_management_request(request: &str) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream =
        UnixStream::connect(socket_path()).context("failed to connect management socket")?;
    stream
        .write_all(request.as_bytes())
        .context("failed to write management request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("failed to read management response")?;
    Ok(response)
}

#[cfg(not(unix))]
fn send_management_shutdown() -> Result<()> {
    // Windows: 通过 TCP localhost 连接管理通道
    use std::io::{Read, Write};
    let addr = management_tcp_addr();
    let mut stream =
        std::net::TcpStream::connect(addr).context("failed to connect management TCP channel")?;
    stream
        .write_all(b"POST /shutdown HTTP/1.1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .context("failed to write management request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("failed to read management response")?;
    if response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        bail!("management shutdown failed")
    }
}

#[cfg(not(unix))]
fn send_management_request(request: &str) -> Result<String> {
    use std::io::{Read, Write};
    let addr = management_tcp_addr();
    let mut stream =
        std::net::TcpStream::connect(addr).context("failed to connect management TCP channel")?;
    stream
        .write_all(request.as_bytes())
        .context("failed to write management request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("failed to read management response")?;
    Ok(response)
}

#[cfg(not(unix))]
fn management_tcp_addr() -> std::net::SocketAddr {
    // TODO: 从配置或 PID 文件读取实际管理通道地址
    "127.0.0.1:8989".parse().unwrap()
}

fn parse_http_json_response(response: &str) -> Result<Value> {
    if !response.starts_with("HTTP/1.1 200") {
        bail!("management request failed");
    }
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .context("management response missing body")?;
    serde_json::from_str(body).context("management response body is not JSON")
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).context("failed to terminate process");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .context("failed to invoke taskkill")?;
    if !status.success() {
        bail!("taskkill failed with {status}");
    }
    Ok(())
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

fn absolute_config_path(config_path: &Path) -> PathBuf {
    if config_path.is_absolute() {
        return config_path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(config_path)
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_server_ownership_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock =
            acquire_server_ownership_in(temp.path(), Duration::from_secs(1)).expect("acquire");
        let meta =
            crate::ownership::OwnershipLock::read_metadata(&temp.path().join("ownership.lock"))
                .expect("metadata");
        assert_eq!(meta.process_type, "server");
        assert_eq!(meta.command, "llm-proxy serve");
        drop(lock);
    }

    #[test]
    fn acquire_server_ownership_fails_on_server_holder() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock =
            crate::ownership::OwnershipLock::try_acquire(temp.path(), "server", "llm-proxy serve")
                .expect("pre-acquire server lock");
        let err = acquire_server_ownership_in(temp.path(), Duration::from_millis(100))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("另一个 llm-proxy server 已持有所有权锁"),
            "{err}"
        );
    }

    #[test]
    fn acquire_server_ownership_waits_for_cli_then_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _lock = crate::ownership::OwnershipLock::try_acquire(
            temp.path(),
            "cli",
            "llm-proxy provider add",
        )
        .expect("pre-acquire cli lock");
        let started = Instant::now();
        let err = acquire_server_ownership_in(temp.path(), Duration::from_millis(250))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(err.contains("所有权锁被 CLI 持有"), "{err}");
        assert!(err.contains("超时"), "{err}");
    }

    #[test]
    fn env_injection_request_has_json_body_and_content_length() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("LLM_PROXY_TEST_KEY".to_string(), "secret".to_string());
        let request = build_env_injection_request(&env).expect("request");
        assert!(request.starts_with("POST /env HTTP/1.1"));
        let body = request.split("\r\n\r\n").nth(1).expect("body");
        assert_eq!(
            serde_json::from_str::<Value>(body).expect("json"),
            json!({"env":{"LLM_PROXY_TEST_KEY":"secret"}})
        );
        assert!(request.contains(&format!("content-length: {}", body.len())));
    }

    #[tokio::test]
    async fn management_state_and_env_handlers_return_json() {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let mgmt = ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(Some(tx))),
        };

        let state = state_handler(State(mgmt.clone())).await;
        assert!(state.0.get("bad_request_blocks").is_some());

        let (status, env) = env_handler(
            State(mgmt.clone()),
            axum::Json(json!({"env": {"LLM_PROXY_TEST_ENV": "ok"}})),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(env.0.to_string().contains("LLM_PROXY_TEST_ENV"));
        assert_eq!(
            std::env::var("LLM_PROXY_TEST_ENV").ok().as_deref(),
            Some("ok")
        );
    }

    #[tokio::test]
    async fn management_shutdown_consumes_sender_once() {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mgmt = ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(Some(tx))),
        };
        let response = shutdown_handler(State(mgmt.clone())).await;
        assert_eq!(response.0["shutdown"], "scheduled");
        assert!(rx.try_recv().is_ok());
        assert!(mgmt.shutdown_tx.lock().expect("lock").is_none());
    }

    #[test]
    fn service_paths_live_in_global_state_dir() {
        assert_eq!(pid_path(), state_dir().join("llm-proxy.pid"));
        assert_eq!(socket_path(), state_dir().join("llm-proxy.sock"));
        assert_eq!(log_path(), state_dir().join("llm-proxy.log"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "sandbox may disallow Unix domain socket bind"]
    async fn uds_management_router_serves_state_and_admin_write_endpoints() {
        use tokio::net::UnixListener;
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-uds-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let sock = temp.path().join("mgmt.sock");
        let listener = UnixListener::bind(&sock).expect("bind uds listener");

        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let router = management_router(app, Arc::new(Mutex::new(Some(tx))));

        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let service = hyper_util::service::TowerToHyperService::new(
                    router.clone().into_service::<hyper::body::Incoming>(),
                );
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });

        // GET /state —— 管理状态端点
        let resp = uds_test_request(&sock, "GET /state HTTP/1.1\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "state: {resp}");
        assert!(resp.contains("bad_request_blocks"));

        // DELETE /admin/provider/remove —— 写端点必须路由到 handler
        // （400 = provider 不存在，证明请求到达 handler 而非 404）
        let body = r#"{"id":"nonexistent","force":false}"#;
        let request = format!(
            "DELETE /admin/provider/remove HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = uds_test_request(&sock, &request).await;
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "admin write endpoint must be routed on UDS, got: {resp}"
        );

        // 未知路径 → 404
        let resp = uds_test_request(&sock, "GET /admin/nonexistent HTTP/1.1\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 404"), "unknown path: {resp}");
    }

    #[cfg(unix)]
    async fn uds_test_request(sock: &std::path::Path, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // hyper 默认 keep-alive；请求显式 connection: close 以便客户端读到 EOF
        let request = request.replace("\r\n\r\n", "\r\nconnection: close\r\n\r\n");
        let mut stream = tokio::net::UnixStream::connect(sock)
            .await
            .expect("connect uds");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");
        response
    }

    #[test]
    fn parse_http_json_response_accepts_200_with_body() {
        let value = parse_http_json_response(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"ok\":true}",
        )
        .expect("json");
        assert_eq!(value, json!({"ok": true}));
    }

    #[test]
    fn parse_http_json_response_rejects_non_200() {
        let err = parse_http_json_response(
            "HTTP/1.1 500 Internal Server Error\r\n\r\n{\"error\":\"boom\"}",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("management request failed"));
    }

    #[test]
    fn parse_http_json_response_rejects_missing_body_separator() {
        let err = parse_http_json_response("HTTP/1.1 200 OK")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing body"));
    }

    #[test]
    fn parse_http_json_response_rejects_invalid_json_body() {
        let err = parse_http_json_response("HTTP/1.1 200 OK\r\n\r\nnot-json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"));
    }

    #[test]
    fn apply_env_body_value_requires_env_object() {
        let err = apply_env_body_value(&json!({})).unwrap_err().to_string();
        assert!(err.contains("missing env object"));
    }

    #[test]
    fn apply_env_body_value_rejects_non_string_value() {
        let err = apply_env_body_value(&json!({"env": {"LLM_PROXY_BAD": 123}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a string"));
    }

    #[test]
    fn apply_env_body_value_applies_multiple_sorted_keys() {
        let value = json!({"env": {"LLM_PROXY_TEST_B": "two", "LLM_PROXY_TEST_A": "one"}});
        let mut applied = apply_env_body_value(&value).expect("apply env");
        applied.sort();
        assert_eq!(applied, vec!["LLM_PROXY_TEST_A", "LLM_PROXY_TEST_B"]);
        assert_eq!(
            std::env::var("LLM_PROXY_TEST_A").ok().as_deref(),
            Some("one")
        );
        assert_eq!(
            std::env::var("LLM_PROXY_TEST_B").ok().as_deref(),
            Some("two")
        );
    }

    #[test]
    fn build_env_injection_request_empty_env_still_builds_valid_post() {
        let env = std::collections::BTreeMap::new();
        let request = build_env_injection_request(&env).expect("request");
        let body = request.split("\r\n\r\n").nth(1).expect("body");
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"env": {}})
        );
        assert!(request.contains(&format!("content-length: {}", body.len())));
    }

    #[test]
    fn inject_env_empty_map_short_circuits_without_socket() {
        let env = std::collections::BTreeMap::new();
        let applied = inject_env(&env).expect("empty env succeeds");
        assert!(applied.is_empty());
    }

    #[test]
    fn absolute_config_path_keeps_absolute_paths() {
        let path = PathBuf::from("/tmp/llm-proxy-test-config.toml");
        assert_eq!(absolute_config_path(&path), path);
    }

    #[test]
    fn absolute_config_path_resolves_relative_to_current_dir() {
        let path = Path::new("relative-config.toml");
        let resolved = absolute_config_path(path);
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with(path));
    }

    #[test]
    #[serial_test::serial]
    fn running_pid_returns_none_when_pid_file_missing_or_invalid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        assert_eq!(running_pid(), None);
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "not-a-pid\n").unwrap();
        assert_eq!(running_pid(), None);
    }

    #[test]
    #[serial_test::serial]
    fn refuse_if_running_removes_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "abc\n").unwrap();
        refuse_if_running().expect("invalid pid is stale");
        assert!(!pid_path().exists());
    }

    #[test]
    #[serial_test::serial]
    fn refuse_if_running_removes_stale_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "99999999\n").unwrap();
        refuse_if_running().expect("stale pid is removed");
        assert!(!pid_path().exists());
    }

    #[test]
    #[serial_test::serial]
    fn refuse_if_running_fails_for_current_process_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), format!("{}\n", std::process::id())).unwrap();
        let err = refuse_if_running().unwrap_err().to_string();
        assert!(err.contains("already running"));
        assert!(pid_path().exists());
    }

    #[test]
    #[serial_test::serial]
    fn write_pid_creates_pid_file_under_state_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        write_pid(4242).expect("write pid");
        assert_eq!(fs::read_to_string(pid_path()).unwrap(), "4242\n");
    }

    #[test]
    fn wait_for_exit_returns_quickly_for_nonexistent_pid() {
        let start = Instant::now();
        wait_for_exit(99999999, Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_process_ignores_nonexistent_pid() {
        terminate_process(99999999).expect("ESRCH is treated as already stopped");
    }

    #[test]
    #[serial_test::serial]
    fn state_dir_env_override_appends_product_subdir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        assert_eq!(state_dir(), temp.path().join("llm-proxy"));
    }

    #[tokio::test]
    async fn env_handler_rejects_bad_body_with_400() {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let mgmt = ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(Some(tx))),
        };
        let (status, body) =
            env_handler(State(mgmt), axum::Json(json!({"env": {"X": false}}))).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("must be a string")
        );
    }

    #[test]
    fn parse_http_json_response_accepts_200_without_reason_phrase() {
        let value = parse_http_json_response("HTTP/1.1 200\r\n\r\n{\"n\":1}").expect("json");
        assert_eq!(value["n"], 1);
    }

    #[test]
    fn parse_http_json_response_accepts_200_with_extra_headers() {
        let value = parse_http_json_response(
            "HTTP/1.1 200 OK\r\ndate: today\r\ncontent-length: 13\r\n\r\n{\"ok\":false}",
        )
        .expect("json");
        assert_eq!(value, json!({"ok": false}));
    }

    #[test]
    fn parse_http_json_response_accepts_json_array_body() {
        let value = parse_http_json_response("HTTP/1.1 200 OK\r\n\r\n[1,2,3]").expect("json");
        assert_eq!(value.as_array().unwrap().len(), 3);
    }

    #[test]
    fn parse_http_json_response_rejects_http_201() {
        let err = parse_http_json_response("HTTP/1.1 201 Created\r\n\r\n{\"ok\":true}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("management request failed"));
    }

    #[test]
    fn parse_http_json_response_rejects_empty_body() {
        let err = parse_http_json_response("HTTP/1.1 200 OK\r\n\r\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"));
    }

    #[test]
    fn apply_env_body_value_rejects_env_array() {
        let err = apply_env_body_value(&json!({"env": []}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing env object"));
    }

    #[test]
    fn apply_env_body_value_rejects_null_value() {
        let err = apply_env_body_value(&json!({"env": {"LLM_PROXY_NULL": null}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("env value for LLM_PROXY_NULL must be a string"));
    }

    #[test]
    fn apply_env_body_value_accepts_empty_env_object() {
        let applied = apply_env_body_value(&json!({"env": {}})).expect("empty env");
        assert!(applied.is_empty());
    }

    #[test]
    fn apply_env_body_value_accepts_empty_string_value() {
        let applied = apply_env_body_value(&json!({"env": {"LLM_PROXY_EMPTY_VALUE": ""}}))
            .expect("apply empty string");
        assert_eq!(applied, vec!["LLM_PROXY_EMPTY_VALUE"]);
        assert_eq!(std::env::var("LLM_PROXY_EMPTY_VALUE").unwrap(), "");
    }

    #[test]
    fn build_env_injection_request_escapes_special_characters() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "LLM_PROXY_SPECIAL".to_string(),
            "line1\nquote\"slash\\".to_string(),
        );
        let request = build_env_injection_request(&env).expect("request");
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"env": {"LLM_PROXY_SPECIAL": "line1\nquote\"slash\\"}})
        );
        assert!(request.contains(&format!("content-length: {}", body.len())));
    }

    #[test]
    fn build_env_injection_request_orders_btree_keys_deterministically() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("B_KEY".to_string(), "b".to_string());
        env.insert("A_KEY".to_string(), "a".to_string());
        let request = build_env_injection_request(&env).expect("request");
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.find("A_KEY").unwrap() < body.find("B_KEY").unwrap());
    }

    #[test]
    fn absolute_config_path_preserves_root_path() {
        let path = PathBuf::from("/");
        assert_eq!(absolute_config_path(&path), PathBuf::from("/"));
    }

    #[test]
    fn absolute_config_path_resolves_dot_relative_path() {
        let resolved = absolute_config_path(Path::new("./config.toml"));
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("config.toml"));
    }

    #[test]
    #[serial_test::serial]
    fn pid_socket_and_log_paths_follow_env_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        assert_eq!(pid_path(), temp.path().join("llm-proxy/llm-proxy.pid"));
        assert_eq!(socket_path(), temp.path().join("llm-proxy/llm-proxy.sock"));
        assert_eq!(log_path(), temp.path().join("llm-proxy/llm-proxy.log"));
    }

    #[test]
    #[serial_test::serial]
    fn running_pid_returns_current_process_when_pid_file_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), format!("{}\n", std::process::id())).unwrap();
        assert_eq!(running_pid(), Some(std::process::id()));
    }

    #[test]
    #[serial_test::serial]
    fn running_pid_returns_none_for_stale_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "99999999\n").unwrap();
        assert_eq!(running_pid(), None);
    }

    #[test]
    #[serial_test::serial]
    fn refuse_if_running_allows_missing_state_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path().join("missing"));
        refuse_if_running().expect("missing pid file is not running");
    }

    #[test]
    #[serial_test::serial]
    fn refuse_if_running_removes_whitespace_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "   \n").unwrap();
        refuse_if_running().expect("blank pid is stale");
        assert!(!pid_path().exists());
    }

    #[test]
    #[serial_test::serial]
    fn write_pid_overwrites_existing_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "1\n").unwrap();
        write_pid(2).expect("write pid");
        assert_eq!(fs::read_to_string(pid_path()).unwrap(), "2\n");
    }

    #[test]
    #[serial_test::serial]
    fn shutdown_background_missing_pid_file_is_ok_when_no_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        shutdown_background(Path::new("/tmp/unused.toml")).expect("not running is ok");
    }

    #[test]
    #[serial_test::serial]
    fn shutdown_background_rejects_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard::set("LLM_PROXY_STATE_DIR", temp.path());
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(pid_path(), "invalid\n").unwrap();
        let err = shutdown_background(Path::new("/tmp/unused.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid pid file"));
    }

    #[test]
    fn wait_for_exit_zero_timeout_returns_immediately() {
        let start = Instant::now();
        wait_for_exit(std::process::id(), Duration::from_millis(0));
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[cfg(unix)]
    #[test]
    fn process_exists_reports_current_process() {
        assert!(process_exists(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn process_exists_reports_false_for_unlikely_pid() {
        assert!(!process_exists(99999999));
    }

    #[tokio::test]
    async fn shutdown_handler_is_ok_when_sender_already_consumed() {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let mgmt = ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(None)),
        };
        let response = shutdown_handler(State(mgmt)).await;
        assert_eq!(response.0, json!({"ok": true, "shutdown": "scheduled"}));
    }

    #[tokio::test]
    async fn shutdown_handler_second_call_does_not_resend() {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mgmt = ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(Some(tx))),
        };
        let _ = shutdown_handler(State(mgmt.clone())).await;
        assert!(rx.try_recv().is_ok());
        let _ = shutdown_handler(State(mgmt.clone())).await;
        assert!(mgmt.shutdown_tx.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn env_handler_accepts_empty_env_object() {
        let mgmt = test_management_state();
        let (status, body) = env_handler(State(mgmt), axum::Json(json!({"env": {}}))).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body.0, json!({"applied": []}));
    }

    #[tokio::test]
    async fn env_handler_reports_missing_env_object() {
        let mgmt = test_management_state();
        let (status, body) = env_handler(State(mgmt), axum::Json(json!({"not_env": {}}))).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            body.0["error"]
                .as_str()
                .unwrap()
                .contains("missing env object")
        );
    }

    #[tokio::test]
    async fn state_handler_returns_object_state() {
        let mgmt = test_management_state();
        let state = state_handler(State(mgmt)).await;
        assert!(state.0.is_object());
    }

    #[test]
    fn management_state_clone_shares_shutdown_mutex() {
        let mgmt = test_management_state();
        let cloned = mgmt.clone();
        assert!(Arc::ptr_eq(&mgmt.shutdown_tx, &cloned.shutdown_tx));
    }

    fn test_management_state() -> ManagementState {
        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        ManagementState {
            app,
            shutdown_tx: Arc::new(Mutex::new(Some(tx))),
        }
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
