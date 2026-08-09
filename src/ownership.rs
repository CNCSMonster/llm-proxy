//! 所有权锁（§15.2 Ownership and Write Serialization）
//!
//! 该模块只提供跨进程所有权锁基础设施。锁是否被持有只以
//! `try_lock_exclusive` 的结果为准；`ownership.lock` 文件内容仅用于诊断。
//!
//! 阶段 1 仅实现基础设施，尚未接入 server/CLI 调用点（后续阶段接入），
//! 故暂时允许 dead_code。

#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
        writeln!(
            f,
            "  持有者: {} (pid={}), 启动于 {}",
            holder, meta.pid, meta.started_at
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
    command: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_cli_write_lock_at(&crate::service::state_dir(), command, f)
}

/// 在指定 state_dir 上执行 CLI 本地写操作（可测试性：传入临时目录，
/// 避免修改全局 `LLM_PROXY_STATE_DIR` 环境变量污染并行测试）。
pub fn with_cli_write_lock_at<T>(
    state_dir: &Path,
    command: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = OwnershipLock::try_acquire(state_dir, "cli", command)?;
    f()
}

/// 异步版本：锁持有跨 await（如 connect 流程含网络验证）。
/// 独立模式（无 server）的验证调用通常毫秒~几秒，可接受。
pub async fn with_cli_write_lock_async<T, F>(command: &str, f: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    with_cli_write_lock_at_async(&crate::service::state_dir(), command, f).await
}

/// 在指定 state_dir 上执行 CLI 本地写操作（异步版本，可测试性同 `_at`）。
pub async fn with_cli_write_lock_at_async<T, F>(
    state_dir: &Path,
    command: &str,
    f: F,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let _lock = OwnershipLock::try_acquire(state_dir, "cli", command)?;
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
    if let Some(server) = crate::admin_client::detect_server(config_path).await? {
        return delegate(&server).await;
    }

    match OwnershipLock::try_acquire(&crate::service::state_dir(), "cli", command) {
        Ok(_lock) => local_write().await,
        Err(AcquireError::HeldByServer { .. }) => {
            if let Some(server) = crate::admin_client::detect_server(config_path).await? {
                return delegate(&server).await;
            }
            anyhow::bail!("所有权锁被 server 持有但 server 不可达；请稍后重试")
        }
        Err(e) => Err(e.into()),
    }
}

impl OwnershipLock {
    /// 尝试获取所有权锁（非阻塞 try_lock）。
    pub fn try_acquire(
        state_dir: &Path,
        process_type: &str,
        command: &str,
    ) -> Result<OwnershipLock, AcquireError> {
        fs::create_dir_all(state_dir).map_err(AcquireError::Io)?;
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
    use serial_test::serial;

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
    #[serial]
    async fn with_cli_write_lock_or_delegate_local_when_no_server() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        let state_dir = tempfile::tempdir().unwrap();
        fs::write(&config_path, "not valid toml").unwrap();
        unsafe { std::env::set_var("LLM_PROXY_STATE_DIR", state_dir.path()) };

        let value = with_cli_write_lock_or_delegate(
            &config_path,
            "llm-proxy test delegate local",
            || async { Ok(123) },
            |_server| Box::pin(async { Ok(999) }),
        )
        .await
        .unwrap();

        unsafe { std::env::remove_var("LLM_PROXY_STATE_DIR") };
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
}
