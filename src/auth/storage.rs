use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::types::*;

/// 加载 OAuth 账号（新格式），跳过无效账号并返回跳过列表
pub fn load_oauth_accounts(path: &Path) -> Result<(OAuthAccounts, Vec<SkippedAccount>)> {
    validate_path_safety(path)?;
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((OAuthAccounts::new(), Vec::new()));
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    check_file_permissions(path);

    let raw: OAuthAccounts = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    if raw.version != 1 {
        bail!(
            "unsupported OAuth accounts version {} in {}",
            raw.version,
            path.display()
        );
    }

    // 逐个账号验证，跳过无效账号
    let mut valid = OAuthAccounts::new();
    let mut skipped = Vec::new();

    for (id, acc) in raw.antigravity {
        match validate_antigravity_account(&id, &acc) {
            Ok(()) => {
                valid.antigravity.insert(id, acc);
            }
            Err(reason) => {
                skipped.push(SkippedAccount {
                    account_type: "antigravity".to_string(),
                    account_id: id,
                    reason: reason.to_string(),
                });
            }
        }
    }

    for (id, acc) in raw.openai {
        match validate_openai_account(&id, &acc) {
            Ok(()) => {
                valid.openai.insert(id, acc);
            }
            Err(reason) => {
                skipped.push(SkippedAccount {
                    account_type: "openai".to_string(),
                    account_id: id,
                    reason: reason.to_string(),
                });
            }
        }
    }

    Ok((valid, skipped))
}

/// 验证单个 Antigravity 账号
pub(super) fn validate_antigravity_account(id: &str, acc: &AntigravityAccount) -> Result<()> {
    // Account ID 格式
    if id.is_empty() {
        bail!("account ID is empty");
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid account ID format");
    }
    if id.len() > 64 {
        bail!("account ID too long");
    }

    // Token 不能相同
    if acc.access_token == acc.refresh_token {
        bail!("access_token and refresh_token are identical");
    }

    // Token 长度
    if acc.access_token.len() < 20 {
        bail!("access_token too short");
    }
    if acc.refresh_token.len() < 20 {
        bail!("refresh_token too short");
    }

    // 时间戳合理性
    if acc.expires_at_unix < 1000000000 {
        bail!("invalid expires_at_unix");
    }
    if acc.updated_at_unix < 1000000000 {
        bail!("invalid updated_at_unix");
    }
    if acc.updated_at_unix > acc.expires_at_unix {
        bail!("updated_at_unix > expires_at_unix");
    }

    // Project ID 格式
    let project_pattern = regex::Regex::new(r"^[a-z][a-z0-9-]{4,28}[a-z0-9]$")?;
    if !project_pattern.is_match(&acc.project_id) {
        bail!("invalid Google Cloud project ID: {}", acc.project_id);
    }

    Ok(())
}

/// 验证单个 OpenAI 账号
pub(super) fn validate_openai_account(id: &str, acc: &OpenaiAccount) -> Result<()> {
    // Account ID 格式
    if id.is_empty() {
        bail!("account ID is empty");
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid account ID format");
    }
    if id.len() > 64 {
        bail!("account ID too long");
    }

    // Token 不能相同
    if acc.access_token == acc.refresh_token {
        bail!("access_token and refresh_token are identical");
    }

    // Token 长度
    if acc.access_token.len() < 20 {
        bail!("access_token too short");
    }
    if acc.refresh_token.len() < 20 {
        bail!("refresh_token too short");
    }

    // 时间戳合理性
    if acc.expires_at_unix < 1000000000 {
        bail!("invalid expires_at_unix");
    }
    if acc.updated_at_unix < 1000000000 {
        bail!("invalid updated_at_unix");
    }
    if acc.updated_at_unix > acc.expires_at_unix {
        bail!("updated_at_unix > expires_at_unix");
    }

    Ok(())
}

/// 加载 OAuth 账号，损坏时按 mtime 倒序遍历所有备份尝试恢复，成功后写回主文件
pub fn load_oauth_accounts_with_recovery(path: &Path) -> Result<OAuthAccounts> {
    match load_oauth_accounts(path) {
        Ok((accounts, skipped)) => {
            if !skipped.is_empty() {
                tracing::warn!(
                    "Skipped {} invalid OAuth account(s) in {}",
                    skipped.len(),
                    path.display()
                );
            }
            Ok(accounts)
        }
        Err(err) => {
            // 版本不匹配（config 比 binary 新）→ 严格拒绝，不尝试恢复
            // 防止用旧备份覆盖新配置
            if err
                .to_string()
                .contains("unsupported OAuth accounts version")
            {
                bail!(
                    "OAuth accounts file version is newer than supported: {}\n\
                     Please upgrade llm-proxy to the latest version.",
                    err
                );
            }

            // 其他错误（JSON 损坏、schema 验证失败等）→ 尝试备份恢复
            let backups = find_backups_newest_first(path);
            if backups.is_empty() {
                return Err(err);
            }
            for backup in &backups {
                match load_oauth_accounts(backup) {
                    Ok((accounts, skipped)) => {
                        if !skipped.is_empty() {
                            tracing::warn!(
                                "Skipped {} invalid OAuth account(s) in backup {}",
                                skipped.len(),
                                backup.display()
                            );
                        }
                        tracing::warn!(
                            "OAuth accounts file {} corrupted ({}), recovered from backup {}",
                            path.display(),
                            err,
                            backup.display()
                        );
                        eprintln!(
                            "Warning: OAuth accounts file corrupted; recovered from backup {}.\n\
                             The backup may contain stale or rotated-out tokens. \
                             Run `llm-proxy provider refresh <provider>` or relogin if requests fail.",
                            backup.display()
                        );
                        // 恢复成功后写回主文件（best-effort，失败不阻塞使用）
                        if let Err(write_err) = save_oauth_accounts(path, &accounts) {
                            tracing::warn!(
                                "failed to write recovered accounts back to {}: {}",
                                path.display(),
                                write_err
                            );
                        }
                        return Ok(accounts);
                    }
                    Err(backup_err) => {
                        tracing::warn!("backup {} also invalid: {}", backup.display(), backup_err);
                    }
                }
            }
            bail!(
                "OAuth accounts file corrupted: {err}; all {} backup(s) also invalid. Manual intervention required.",
                backups.len()
            );
        }
    }
}

/// 拒绝文件本身为符号链接；父目录为符号链接属合法用法（dotfiles 管理），仅警告
pub(super) fn validate_path_safety(path: &Path) -> Result<()> {
    if path.is_symlink() {
        bail!(
            "OAuth accounts path is a symbolic link: {}; symbolic links are not allowed for security reasons",
            path.display()
        );
    }
    if let Some(parent) = path.parent()
        && parent.is_symlink()
    {
        tracing::warn!(
            "OAuth accounts parent directory is a symbolic link: {}; allowed, but ensure the target is trusted",
            parent.display()
        );
    }
    Ok(())
}

/// 检查文件权限是否过于宽松（仅 Unix 有效）
fn check_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = fs::metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "OAuth accounts file {} has insecure permissions: {:o}. \
                     Expected: 600. Fix with: chmod 600 {}",
                    path.display(),
                    mode,
                    path.display()
                );
            }
        }
    }
}

/// 获取目录级写锁（排他锁，超时等待）。
/// 锁文件为 `data.lock`，放在数据文件所在目录，全局一把锁保护目录内所有数据文件。
/// 超时行为与 `ConfigLock` 一致，避免某个持锁进程卡死时无限阻塞调用方。
pub(super) fn acquire_lock(path: &Path) -> Result<fs::File> {
    acquire_lock_with_timeout(path, Duration::from_secs(5))
}

pub(super) fn acquire_lock_with_timeout(path: &Path, timeout: Duration) -> Result<fs::File> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let lock_path = parent.join("data.lock");
    let lock_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    use fs2::FileExt;
    let start = std::time::Instant::now();
    loop {
        if lock_file.try_lock_exclusive().is_ok() {
            return Ok(lock_file);
        }
        if start.elapsed() > timeout {
            bail!(
                "timed out waiting for lock on {} (timeout: {}s)",
                lock_path.display(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 锁内执行"读-改"事务；调用方决定是否通过 save_oauth_accounts_locked 写回。
/// 锁覆盖整个 load→modify→save 临界区，防止跨进程丢更新。
pub(crate) fn with_locked_accounts<T>(
    path: &Path,
    f: impl FnOnce(&mut OAuthAccounts) -> Result<T>,
) -> Result<T> {
    validate_path_safety(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock_file = acquire_lock(path)?;
    let _guard = scopeguard::guard(lock_file, |file| {
        let _ = file.unlock();
    });
    let (mut accounts, _skipped) = load_oauth_accounts(path)?;
    f(&mut accounts)
}

/// 写入前备份现有文件，保留最近 3 个备份
fn backup_oauth_accounts(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup_path = backup_path_for(path, timestamp);
    fs::copy(path, &backup_path).with_context(|| {
        format!(
            "failed to back up {} to {}",
            path.display(),
            backup_path.display()
        )
    })?;
    cleanup_old_backups(path, 3)?;
    Ok(())
}

pub(super) fn backup_path_for(path: &Path, timestamp: u128) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".bak.{timestamp}"));
    PathBuf::from(name)
}

pub(super) fn find_backups_newest_first(path: &Path) -> Vec<PathBuf> {
    let Some(prefix) = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.bak."))
    else {
        return Vec::new();
    };
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut backups: Vec<_> = fs::read_dir(parent)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(&prefix))
                })
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    backups.sort_by_key(|path| std::cmp::Reverse(path.metadata().and_then(|m| m.modified()).ok()));
    backups
}

pub(super) fn cleanup_old_backups(path: &Path, keep: usize) -> Result<()> {
    let mut backups = find_backups_newest_first(path);
    if backups.len() > keep {
        for old in backups.drain(keep..) {
            // 清理失败不阻塞写入（可能与其他进程的清理竞态）
            if let Err(err) = fs::remove_file(&old)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("failed to remove old backup {}: {}", old.display(), err);
            }
        }
    }
    Ok(())
}

/// 原子写入：临时文件（0600）+ fsync + rename + 目录 fsync。调用方必须持有写锁。
pub(crate) fn save_oauth_accounts_locked(path: &Path, accounts: &OAuthAccounts) -> Result<()> {
    // 写入前备份现有文件
    backup_oauth_accounts(path)?;

    let json = serde_json::to_string_pretty(accounts)?;
    let temp_path = path.with_extension("tmp");

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&temp_path, &json)?;
    }

    fs::rename(&temp_path, path)?;

    // 目录 fsync，确保 rename 持久化（best-effort）
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// 保存 OAuth 账号（新格式）。自带写锁；已持锁场景请用 with_locked_accounts + save_oauth_accounts_locked。
pub fn save_oauth_accounts(path: &Path, accounts: &OAuthAccounts) -> Result<()> {
    validate_path_safety(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock_file = acquire_lock(path)?;
    let _guard = scopeguard::guard(lock_file, |file| {
        let _ = file.unlock();
    });
    save_oauth_accounts_locked(path, accounts)
}

pub fn default_state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("llm-proxy")
        .join("oauth_accounts.json")
}
