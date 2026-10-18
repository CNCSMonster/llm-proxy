//! 所有权锁（§15.2 Ownership and Write Serialization）
//!
//! 该模块只提供跨进程所有权锁基础设施。锁是否被持有只以
//! `try_lock_exclusive` 的结果为准；`ownership.lock` 文件内容仅用于诊断。
//!
//! 仅实现基础设施，尚未接入 server/CLI 调用点，
//! 故暂时允许 dead_code。

#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// 持有者元数据（仅诊断用，不参与持有判定）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerMetadata {
    pub pid: u32,
    pub process_type: String,
    pub started_at: u64,
    pub command: String,
}

/// 获取失败的原因（区分持有者类型，用于报错指引）
#[derive(Debug)]
pub enum AcquireError {
    HeldByServer { metadata: Option<OwnerMetadata> },
    HeldByCli { metadata: Option<OwnerMetadata> },
    UnknownHolder { metadata: Option<OwnerMetadata> },
    PathUnsafe(String),
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeldByServer { metadata } => write_held(f, "Server", metadata.as_ref()),
            Self::HeldByCli { metadata } => write_held(f, "CLI", metadata.as_ref()),
            Self::UnknownHolder { metadata } => write_held(f, "未知持有者", metadata.as_ref()),
            Self::PathUnsafe(msg) => write!(f, "ownership.lock 路径不安全: {msg}"),
            Self::Io(err) => write!(f, "ownership.lock I/O 错误: {err}"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

fn write_held(
    f: &mut std::fmt::Formatter<'_>,
    holder: &str,
    metadata: Option<&OwnerMetadata>,
) -> std::fmt::Result {
    writeln!(f, "Error: 写入权被占用")?;
    if let Some(meta) = metadata {
        let alive = OwnershipLock::pid_alive(meta.pid);
        writeln!(
            f,
            "  持有者: {} (pid={}, {}), 启动于 {}",
            holder,
            meta.pid,
            if alive { "alive" } else { "dead" },
            meta.started_at
        )?;
        writeln!(f, "  命令: {}", meta.command)?;
    } else {
        writeln!(f, "  持有者: 持有者信息不可读")?;
    }
    writeln!(f, "  可能原因: 该进程卡死（写操作未在 10s 内完成）")?;
    writeln!(f, "  解决:")?;
    writeln!(f, "    1. 等待数秒后重试（CLI 写操作通常毫秒级）")?;
    writeln!(f, "    2. 若持续占用，确认持有者进程已退出")?;
    write!(
        f,
        "    3. 确认没有任何 llm-proxy 进程在运行后，可手动删除锁文件"
    )
}

/// 所有权锁（持有中）。flock 随 File drop 自动释放；不删除锁文件。
#[derive(Debug)]
pub struct OwnershipLock {
    file: File,
    path: PathBuf,
}

/// CLI 本地写操作：先获取所有权锁（cli），成功执行同步闭包，失败返回错误。
/// §15.2 5 步流程第 4 步：无 server 时 try_lock → 写 → 释放。
pub fn with_cli_write_lock<T>(
    config_path: &std::path::Path,
    command: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    with_cli_write_lock_at(&dirs.runtime_dir, command, f)
}

/// 在指定 runtime_dir 上执行 CLI 本地写操作，供测试传入临时目录。
pub fn with_cli_write_lock_at<T>(
    runtime_dir: &Path,
    command: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = OwnershipLock::try_acquire(runtime_dir, "cli", command)?;
    f()
}

/// 异步版本：锁持有跨 await（如 connect 流程含网络验证）。
/// 独立模式（无 server）的验证调用通常毫秒~几秒，可接受。
pub async fn with_cli_write_lock_async<T, F>(
    config_path: &std::path::Path,
    command: &str,
    f: F,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let dirs = crate::service::StateDirs::resolve(config_dir);
    with_cli_write_lock_at_async(&dirs.runtime_dir, command, f).await
}

/// 在指定 runtime_dir 上执行 CLI 本地写操作（异步版本）。
pub async fn with_cli_write_lock_at_async<T, F>(
    runtime_dir: &Path,
    command: &str,
    f: F,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let _lock = OwnershipLock::try_acquire(runtime_dir, "cli", command)?;
    f.await
}

/// §15.2 CLI 写操作完整 5 步流程：
/// 1. detect_server → 2. 有 server → 委托；3. 无 server → try_lock；
/// 4. 获取 → 本地写；5. 未获取（holder=server）→ 重试 detect_server → 委托。
pub async fn with_cli_write_lock_or_delegate<T, Fut, F, D>(
    config_path: &Path,
    command: &str,
    local_write: F,
    delegate: D,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
    D: for<'a> FnOnce(
        &'a crate::admin_client::ServerConnection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>,
    >,
{
    with_cli_write_lock_or_delegate_at(
        config_path,
        &{
            let config_dir = config_path.parent().unwrap_or(std::path::Path::new("."));
            crate::service::StateDirs::resolve(config_dir).runtime_dir
        },
        command,
        local_write,
        delegate,
    )
    .await
}

pub async fn with_cli_write_lock_or_delegate_at<T, Fut, F, D>(
    config_path: &Path,
    runtime_dir: &Path,
    command: &str,
    local_write: F,
    delegate: D,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
    D: for<'a> FnOnce(
        &'a crate::admin_client::ServerConnection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>,
    >,
{
    if let Some(server) = crate::admin_client::detect_server_at(config_path, runtime_dir).await? {
        ensure_same_config(&server, config_path)?;
        return delegate(&server).await;
    }

    match OwnershipLock::try_acquire(runtime_dir, "cli", command) {
        Ok(_lock) => local_write().await,
        Err(AcquireError::HeldByServer { .. }) => {
            if let Some(server) =
                crate::admin_client::detect_server_at(config_path, runtime_dir).await?
            {
                ensure_same_config(&server, config_path)?;
                return delegate(&server).await;
            }
            anyhow::bail!("所有权锁被 server 持有但 server 不可达；请稍后重试")
        }
        Err(e) => Err(e.into()),
    }
}

/// 拒绝把写操作委托给"监听同一地址、但用的是另一份 config"的 server。
///
/// 没有这道检查时，隔离 config 若沿用了与运行中 server 相同的 `listen`，写会
/// **静默落到那份 config 上并报成功**：CLI 的 `--config` 被忽略，
/// 用户以为改的是隔离环境，实际改的是正式配置。
///
/// 不匹配时按 spec.md §11.8「严禁本地降级」处理：不回落本地写（那会绕过 server
/// 持有的 ownership.lock），而是硬失败并给出可操作的诊断。
pub fn ensure_same_config(
    server: &crate::admin_client::ServerConnection,
    config_path: &Path,
) -> anyhow::Result<()> {
    let expected = crate::config::config_identity(config_path.parent().unwrap_or(config_path));
    match server.config_identity.as_deref() {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => anyhow::bail!(
            "拒绝委托：{} 上的 server 正在使用另一份 config（identity {actual}，本命令期望 \
             {expected}）。请为该 config 指定独立的 listen 端口，或先停止另一个 server。",
            server.base_url
        ),
        None => anyhow::bail!(
            "拒绝委托：{} 上的 server 未上报 config identity，无法确认它是本 config 的 server。\
             请升级 server 后重试。",
            server.base_url
        ),
    }
}

impl OwnershipLock {
    /// 尝试获取所有权锁（非阻塞 try_lock）。
    pub fn try_acquire(
        state_dir: &Path,
        process_type: &str,
        command: &str,
    ) -> Result<OwnershipLock, AcquireError> {
        crate::service::ensure_private_state_dir(state_dir)
            .map_err(|e| AcquireError::Io(std::io::Error::other(e.to_string())))?;
        let path = state_dir.join("ownership.lock");
        validate_path_safety(&path)?;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(AcquireError::Io)?;

        if let Err(err) = file.try_lock_exclusive() {
            let metadata = Self::read_metadata(&path);
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(classify_holder(metadata));
            }
            return Err(AcquireError::Io(err));
        }

        let metadata = OwnerMetadata {
            pid: std::process::id(),
            process_type: process_type.to_string(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            command: command.to_string(),
        };
        let data = serde_json::to_vec_pretty(&metadata).map_err(|err| {
            AcquireError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
        })?;
        file.set_len(0).map_err(AcquireError::Io)?;
        file.seek(SeekFrom::Start(0)).map_err(AcquireError::Io)?;
        file.write_all(&data).map_err(AcquireError::Io)?;
        file.write_all(b"\n").map_err(AcquireError::Io)?;
        file.sync_all().map_err(AcquireError::Io)?;

        Ok(Self { file, path })
    }

    /// 读取持有者元数据（尽力而为，诊断用；损坏/部分写入返回 None）。
    pub fn read_metadata(path: &Path) -> Option<OwnerMetadata> {
        let data = fs::read(path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// 检查 pid 进程是否存活。
    pub fn pid_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        pid_alive_impl(pid)
    }
}

impl Drop for OwnershipLock {
    fn drop(&mut self) {
        self.file.unlock().ok();
    }
}

/// Server 独占配置锁（强制单实例独占同一份 config 目录）
///
/// 锁文件位于 `<config_dir>/llm-proxy.server.lock`。
/// 关键约束：不得与 runtime_dir 上的 ownership.lock 混用，两者生命周期与键正交。
#[derive(Debug)]
pub struct ServerConfigLock {
    file: File,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigLockMetadata {
    pub pid: u32,
    pub identity: String,
    pub config_path: String,
    pub started_at: u64,
    pub listen: String,
}

impl ServerConfigLock {
    pub fn config_lock_path(config_path: &Path) -> PathBuf {
        let abs_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(config_path)
        };
        let config_dir = abs_path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| abs_path.parent().unwrap_or(Path::new(".")).to_path_buf());
        config_dir.join("llm-proxy.server.lock")
    }

    pub fn acquire(config_path: &Path, listen: &str) -> anyhow::Result<Self> {
        let abs_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(config_path)
        };
        let config_dir = abs_path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| abs_path.parent().unwrap_or(Path::new(".")).to_path_buf());

        fs::create_dir_all(&config_dir).map_err(|e| {
            anyhow::anyhow!("failed to create config dir {}: {e}", config_dir.display())
        })?;

        let lock_path = config_dir.join("llm-proxy.server.lock");
        if lock_path.is_symlink() {
            anyhow::bail!(
                "{} is a symbolic link; symbolic links are not allowed for llm-proxy.server.lock",
                lock_path.display()
            );
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open config lock file {}", lock_path.display()))?;

        if let Err(err) = file.try_lock_exclusive() {
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let metadata = Self::read_metadata(&lock_path);
                let identity = crate::config::config_identity(&config_dir);
                let (pid_str, listen_str) = match metadata.as_ref() {
                    Some(m) => {
                        let alive_str = if OwnershipLock::pid_alive(m.pid) {
                            "alive"
                        } else {
                            "dead/unknown"
                        };
                        (format!("{} ({alive_str})", m.pid), m.listen.clone())
                    }
                    None => ("unknown".to_string(), "unknown".to_string()),
                };

                anyhow::bail!(
                    "拒绝启动：另一 llm-proxy server 实例（PID: {pid_str}，identity: {identity}，listen: {listen_str}）正在使用同一份 config 目录（{}）。\n\
                     如需运行多实例，请为新实例指定独立 config 目录（--config）与独立 state 目录（XDG_STATE_HOME），或先停止运行中的实例。",
                    config_dir.display()
                );
            }
            return Err(err)
                .with_context(|| format!("failed to acquire flock on {}", lock_path.display()));
        }

        let metadata = ServerConfigLockMetadata {
            pid: std::process::id(),
            identity: crate::config::config_identity(&config_dir),
            config_path: abs_path.display().to_string(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            listen: listen.to_string(),
        };

        let data = serde_json::to_vec_pretty(&metadata)
            .context("failed to serialize server config lock metadata")?;
        file.set_len(0)
            .context("failed to truncate config lock file")?;
        file.seek(SeekFrom::Start(0))
            .context("failed to seek config lock file")?;
        file.write_all(&data)
            .context("failed to write config lock metadata")?;
        file.write_all(b"\n")
            .context("failed to write newline to config lock file")?;
        file.sync_all().context("failed to sync config lock file")?;

        Ok(Self {
            file,
            path: lock_path,
        })
    }

    pub fn preflight_check(config_path: &Path) -> anyhow::Result<()> {
        let abs_path = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(config_path)
        };
        let config_dir = abs_path
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| abs_path.parent().unwrap_or(Path::new(".")).to_path_buf());

        let lock_path = config_dir.join("llm-proxy.server.lock");
        if !lock_path.exists() {
            return Ok(());
        }

        let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).context("preflight config lock check failed to open lock file");
            }
        };

        if let Err(err) = file.try_lock_exclusive()
            && err.kind() == std::io::ErrorKind::WouldBlock
        {
            let metadata = Self::read_metadata(&lock_path);
            let identity = crate::config::config_identity(&config_dir);
            let (pid_str, listen_str) = match metadata.as_ref() {
                Some(m) => {
                    let alive_str = if OwnershipLock::pid_alive(m.pid) {
                        "alive"
                    } else {
                        "dead/unknown"
                    };
                    (format!("{} ({alive_str})", m.pid), m.listen.clone())
                }
                None => ("unknown".to_string(), "unknown".to_string()),
            };

            anyhow::bail!(
                "拒绝启动：另一 llm-proxy server 实例（PID: {pid_str}，identity: {identity}，listen: {listen_str}）正在使用同一份 config 目录（{}）。\n\
                 如需运行多实例，请为新实例指定独立 config 目录（--config）与独立 state 目录（XDG_STATE_HOME），或先停止运行中的实例。",
                config_dir.display()
            );
        }
        file.unlock().ok();
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_metadata(path: &Path) -> Option<ServerConfigLockMetadata> {
        let data = fs::read(path).ok()?;
        serde_json::from_slice(&data).ok()
    }
}

impl Drop for ServerConfigLock {
    fn drop(&mut self) {
        self.file.unlock().ok();
    }
}

fn classify_holder(metadata: Option<OwnerMetadata>) -> AcquireError {
    match metadata.as_ref().map(|m| m.process_type.as_str()) {
        Some("server") => AcquireError::HeldByServer { metadata },
        Some("cli") => AcquireError::HeldByCli { metadata },
        _ => AcquireError::UnknownHolder { metadata },
    }
}

fn validate_path_safety(path: &Path) -> Result<(), AcquireError> {
    if path.is_symlink() {
        return Err(AcquireError::PathUnsafe(format!(
            "{} is a symbolic link; symbolic links are not allowed for ownership.lock",
            path.display()
        )));
    }
    if let Some(parent) = path.parent()
        && parent.is_symlink()
    {
        tracing::warn!(
            "ownership.lock parent directory is a symbolic link: {}; allowed, but ensure the target is trusted",
            parent.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn pid_alive_impl(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn pid_alive_impl(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 as _ {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-proxy-ownership-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn with_cli_write_lock_success() {
        let dir = tempfile::tempdir().unwrap();
        let value = with_cli_write_lock_at(dir.path(), "llm-proxy test", || Ok(42)).unwrap();
        assert_eq!(value, 42);
        let meta = OwnershipLock::read_metadata(&dir.path().join("ownership.lock")).unwrap();
        assert_eq!(meta.process_type, "cli");
        assert_eq!(meta.command, "llm-proxy test");
    }

    #[test]
    fn with_cli_write_lock_fails_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = OwnershipLock::try_acquire(dir.path(), "server", "llm-proxy serve").unwrap();
        let err = with_cli_write_lock_at(dir.path(), "llm-proxy test", || Ok(())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("写入权被占用"));
        assert!(msg.contains("Server"));
    }

    #[tokio::test]
    async fn with_cli_write_lock_async_success() {
        let dir = tempfile::tempdir().unwrap();
        let value =
            with_cli_write_lock_at_async(dir.path(), "llm-proxy async test", async { Ok(7) })
                .await
                .unwrap();
        assert_eq!(value, 7);
        let meta = OwnershipLock::read_metadata(&dir.path().join("ownership.lock")).unwrap();
        assert_eq!(meta.process_type, "cli");
        assert_eq!(meta.command, "llm-proxy async test");
    }
    #[tokio::test]
    async fn with_cli_write_lock_or_delegate_local_when_no_server() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        let runtime_dir = tempfile::tempdir().unwrap();
        fs::write(&config_path, "not valid toml").unwrap();

        let value = with_cli_write_lock_or_delegate_at(
            &config_path,
            runtime_dir.path(),
            "llm-proxy test delegate local",
            || async { Ok(123) },
            |_server| Box::pin(async { Ok(999) }),
        )
        .await
        .unwrap();

        assert_eq!(value, 123);
    }

    #[test]
    fn held_by_server_error_when_try_lock_fails() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = OwnershipLock::try_acquire(dir.path(), "server", "llm-proxy serve").unwrap();
        let err = OwnershipLock::try_acquire(dir.path(), "cli", "llm-proxy model add")
            .expect_err("second exclusive lock must fail");
        assert!(matches!(err, AcquireError::HeldByServer { .. }));
    }

    #[test]
    fn try_acquire_success_writes_metadata() {
        let dir = temp_state("success");
        let lock =
            OwnershipLock::try_acquire(&dir, "cli", "llm-proxy provider add deepseek").unwrap();
        let meta = OwnershipLock::read_metadata(&dir.join("ownership.lock")).unwrap();
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.process_type, "cli");
        assert_eq!(meta.command, "llm-proxy provider add deepseek");
        drop(lock);
    }

    #[test]
    fn try_acquire_failure_reports_holder_type() {
        let dir = temp_state("held");
        let _lock = OwnershipLock::try_acquire(&dir, "server", "llm-proxy serve").unwrap();
        let err = OwnershipLock::try_acquire(&dir, "cli", "llm-proxy provider add").unwrap_err();
        assert!(matches!(
            err,
            AcquireError::HeldByServer { metadata: Some(_) }
        ));
    }

    #[test]
    fn reacquire_after_drop_succeeds() {
        let dir = temp_state("reacquire");
        let lock = OwnershipLock::try_acquire(&dir, "cli", "first").unwrap();
        drop(lock);
        let _lock = OwnershipLock::try_acquire(&dir, "cli", "second").unwrap();
        let meta = OwnershipLock::read_metadata(&dir.join("ownership.lock")).unwrap();
        assert_eq!(meta.command, "second");
    }

    #[test]
    fn stale_lock_file_without_flock_does_not_block() {
        let dir = temp_state("stale");
        fs::write(dir.join("ownership.lock"), b"stale").unwrap();
        let _lock = OwnershipLock::try_acquire(&dir, "cli", "fresh").unwrap();
        let meta = OwnershipLock::read_metadata(&dir.join("ownership.lock")).unwrap();
        assert_eq!(meta.command, "fresh");
    }

    #[test]
    fn corrupted_metadata_returns_none() {
        let dir = temp_state("corrupt");
        let path = dir.join("ownership.lock");
        fs::write(&path, b"not json").unwrap();
        assert!(OwnershipLock::read_metadata(&path).is_none());
    }

    #[test]
    fn pid_alive_detects_current_and_missing_pid() {
        assert!(OwnershipLock::pid_alive(std::process::id()));
        assert!(!OwnershipLock::pid_alive(0));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_path_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = temp_state("symlink");
        let target = dir.join("target.lock");
        fs::write(&target, b"").unwrap();
        symlink(&target, dir.join("ownership.lock")).unwrap();

        let err = OwnershipLock::try_acquire(&dir, "cli", "cmd").unwrap_err();
        assert!(matches!(err, AcquireError::PathUnsafe(_)));
    }

    /// Start a minimal UDS mock server that responds to GET /admin/ping with a compatible
    /// version and the given config identity. The socket is placed at the canonical
    /// `llm-proxy.sock` path inside the given runtime_dir.
    #[cfg(unix)]
    async fn start_mock_ping_server_at(
        config_identity: String,
        runtime_dir: &std::path::Path,
    ) -> tokio::task::JoinHandle<()> {
        let sock = crate::service::socket_path_in(runtime_dir);
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind mock uds");
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/admin/ping",
                axum::routing::get(move || {
                    let identity = config_identity.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "version": env!("CARGO_PKG_VERSION"),
                            "config_identity": identity,
                        }))
                    }
                }),
            );
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
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
        })
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn delegate_error_does_not_run_local_write() {
        // config 必须先存在才能算身份；身份只由路径决定，因此之后把 listen
        // 改写成 mock 的地址不会改变它。
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").unwrap();
        let identity = crate::config::config_identity(config_dir.path());

        let runtime_dir = tempfile::tempdir().unwrap();
        let _server_handle = start_mock_ping_server_at(identity, runtime_dir.path()).await;

        let local_write_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = local_write_called.clone();

        let result: anyhow::Result<i32> = with_cli_write_lock_or_delegate_at(
            &config_path,
            runtime_dir.path(),
            "llm-proxy test delegate error",
            || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(42) }
            },
            |_server| Box::pin(async { Err(anyhow::anyhow!("simulated delegation failure")) }),
        )
        .await;

        assert!(result.is_err(), "expected Err from delegate");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated delegation failure")
        );
        assert!(
            !local_write_called.load(std::sync::atomic::Ordering::SeqCst),
            "local_write must NOT run when delegate is used"
        );
    }

    /// 回归：监听同一地址、但用的是**另一份** config 的 server，写操作
    /// 必须被拒绝。
    ///
    /// 修复之前只比对 `listen` 与版本，于是隔离 config 若沿用了与运行中 server
    /// 相同的端口，写会静默落到那份 config 上并报成功 —— CLI 的 `--config` 被
    /// 忽略。本用例断言两件事：委托**没有**发生（否则写去了别处），且**没有**
    /// 回落本地写（spec.md §11.8「严禁本地降级」）。
    #[tokio::test]
    #[cfg(unix)]
    async fn delegation_to_a_server_using_a_different_config_is_refused() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:0\"\n").unwrap();

        let runtime_dir = tempfile::tempdir().unwrap();
        let _server_handle =
            start_mock_ping_server_at("00000000deadbeef".to_string(), runtime_dir.path()).await;

        let local_write_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delegate_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lw = local_write_called.clone();
        let dc = delegate_called.clone();

        let result: anyhow::Result<i32> = with_cli_write_lock_or_delegate_at(
            &config_path,
            runtime_dir.path(),
            "llm-proxy test config identity mismatch",
            || {
                lw.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(42) }
            },
            |_server| {
                dc.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok(7) })
            },
        )
        .await;

        let err = result
            .expect_err("delegating to a server with a different config must fail")
            .to_string();
        assert!(err.contains("另一份 config"), "unexpected error: {err}");
        assert!(
            !delegate_called.load(std::sync::atomic::Ordering::SeqCst),
            "must not delegate to a server that owns a different config"
        );
        assert!(
            !local_write_called.load(std::sync::atomic::Ordering::SeqCst),
            "must not fall back to a local write either (§11.8 严禁本地降级)"
        );
    }

    #[test]
    fn ensure_same_config_contract() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8989\"\n").unwrap();
        let expected_identity = crate::config::config_identity(dir.path());

        // 1. 匹配的 identity → Ok(())
        let matching_server = crate::admin_client::ServerConnection {
            base_url: "http://127.0.0.1:8989".to_string(),
            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            config_identity: Some(expected_identity.clone()),
            uds_path: std::path::PathBuf::from("/tmp/llm-proxy.sock"),
            management_port: 8989,
        };
        assert!(ensure_same_config(&matching_server, &config_path).is_ok());

        // 2. 不匹配的 identity → 明确报错并包含双方 identity 与指引
        let mismatched_server = crate::admin_client::ServerConnection {
            base_url: "http://127.0.0.1:8989".to_string(),
            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            config_identity: Some("different_hash".to_string()),
            uds_path: std::path::PathBuf::from("/tmp/llm-proxy.sock"),
            management_port: 8989,
        };
        let err = ensure_same_config(&mismatched_server, &config_path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("拒绝委托"), "must reject: {err}");
        assert!(
            err.contains("正在使用另一份 config"),
            "must state reason: {err}"
        );
        assert!(
            err.contains("different_hash"),
            "must report actual identity: {err}"
        );
        assert!(
            err.contains(&expected_identity),
            "must report expected identity: {err}"
        );

        // 3. 缺失 identity（未上报）→ 拒绝并提示升级 server
        let missing_identity_server = crate::admin_client::ServerConnection {
            base_url: "http://127.0.0.1:8989".to_string(),
            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            config_identity: None,
            uds_path: std::path::PathBuf::from("/tmp/llm-proxy.sock"),
            management_port: 8989,
        };
        let err_missing = ensure_same_config(&missing_identity_server, &config_path)
            .unwrap_err()
            .to_string();
        assert!(
            err_missing.contains("拒绝委托"),
            "must reject: {err_missing}"
        );
        assert!(
            err_missing.contains("未上报 config identity"),
            "must report missing identity: {err_missing}"
        );
    }

    #[test]
    fn server_config_lock_acquire_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8989\"\n").unwrap();

        let lock = ServerConfigLock::acquire(&config_path, "127.0.0.1:8989").expect("acquire");
        assert!(lock.path().exists());

        let meta = ServerConfigLock::read_metadata(lock.path()).expect("metadata");
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.listen, "127.0.0.1:8989");
        assert_eq!(meta.identity, crate::config::config_identity(dir.path()));
        drop(lock);

        // After drop, subsequent acquire must succeed
        let lock2 =
            ServerConfigLock::acquire(&config_path, "127.0.0.1:8989").expect("second acquire");
        drop(lock2);
    }

    #[test]
    fn server_config_lock_conflict_fails_with_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8989\"\n").unwrap();

        let lock =
            ServerConfigLock::acquire(&config_path, "127.0.0.1:8989").expect("first acquire");

        // Second acquire on the same config must fail
        let err = ServerConfigLock::acquire(&config_path, "127.0.0.1:9090")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("拒绝启动"),
            "error must state startup refused: {err}"
        );
        assert!(
            err.contains("正在使用同一份 config 目录"),
            "error must identify config conflict: {err}"
        );
        assert!(
            err.contains(&std::process::id().to_string()),
            "error must contain holder pid: {err}"
        );
        assert!(
            err.contains("127.0.0.1:8989"),
            "error must contain holder listen address: {err}"
        );

        // Preflight check must also fail
        let preflight_err = ServerConfigLock::preflight_check(&config_path)
            .unwrap_err()
            .to_string();
        assert!(
            preflight_err.contains("拒绝启动"),
            "preflight error must state startup refused: {preflight_err}"
        );

        drop(lock);

        // Preflight and acquire should now succeed
        ServerConfigLock::preflight_check(&config_path).expect("preflight after drop");
        let _lock3 =
            ServerConfigLock::acquire(&config_path, "127.0.0.1:9090").expect("acquire after drop");
    }

    #[test]
    fn server_config_lock_recovers_from_stale_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8989\"\n").unwrap();

        let lock_path = dir.path().join("llm-proxy.server.lock");
        fs::write(&lock_path, b"{\"pid\": 999999, \"identity\": \"deadbeef\", \"config_path\": \"\", \"started_at\": 0, \"listen\": \"127.0.0.1:8989\"}\n").unwrap();

        // Stale lock file without OS flock must not block acquisition
        let lock = ServerConfigLock::acquire(&config_path, "127.0.0.1:8989")
            .expect("acquire over stale file");
        let meta = ServerConfigLock::read_metadata(lock.path()).expect("updated metadata");
        assert_eq!(meta.pid, std::process::id());
        drop(lock);
    }

    #[test]
    fn server_config_lock_independent_across_different_configs() {
        let dir1 = tempfile::tempdir().unwrap();
        let config_path1 = dir1.path().join("config.toml");
        fs::write(&config_path1, "[server]\nlisten = \"127.0.0.1:8989\"\n").unwrap();

        let dir2 = tempfile::tempdir().unwrap();
        let config_path2 = dir2.path().join("config.toml");
        fs::write(&config_path2, "[server]\nlisten = \"127.0.0.1:9090\"\n").unwrap();

        let lock1 = ServerConfigLock::acquire(&config_path1, "127.0.0.1:8989").expect("lock1");
        let lock2 = ServerConfigLock::acquire(&config_path2, "127.0.0.1:9090").expect("lock2");

        assert_ne!(lock1.path(), lock2.path());
        drop(lock1);
        drop(lock2);
    }
}
