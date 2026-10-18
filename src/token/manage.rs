//! `llm-proxy token` 命令的 token 生命周期管理（§5）。
//!
//! 所有操作作用于 `tokens.toml`（`config.toml` 的同目录兄弟文件）与状态目录下
//! 的配额 SQLite。每次写入都在进程级写锁（[`TokenStoreGuard`]）下完成整段
//! 读-改-写；CLI 本地写另持有跨进程 ownership flock；两层锁都罩不住的写者，由
//! 原子替换**之前和之后**各比一次文件内容来发现（§5.1、§6）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::config::{ModelQuotaGroup, QuotaWindow, TokenConfig, TokensConfig};
use super::keygen::generate_key;
use super::usage::{TokenUsageStore, WindowUsage};

/// 以给定的 `config.toml` 路径解析其同目录的 `tokens.toml`。
pub fn tokens_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.join("tokens.toml"))
        .unwrap_or_else(|| PathBuf::from("tokens.toml"))
}

/// `tokens.toml` 的进程级写锁。
///
/// 这个文件只有一个写者实现——本模块——所以锁放在这里而不是放进每个调用方：
/// 下面的写入口一律要求传入 [`TokenStoreGuard`]，"忘了加锁"因此是类型错误，
/// 而不是一条靠纪律的约定。
///
/// 作用域是**单个进程**。跨进程的写由另外两层负责：CLI 在整段本地写期间持有
/// ownership flock；不属于我们这把锁的写者，由 [`mutate_tokens`] 的写前内容
/// hash 检查与写后复核兜住。多个实例共享同一 config 目录的情形由产品前提排除
/// （D6；该前提目前未被强制，缺口已另行登记）。
static TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 调用方持有 `tokens.toml` 写锁的凭据。
///
/// 由 [`lock_token_store`] 取得；从获取到丢弃之间应当覆盖整段读-改-写、内存
/// reload 以及配额行清算。它不是 `Send`：只在同步代码段里持有，绝不跨越
/// `.await`。
pub struct TokenStoreGuard {
    /// 从不被读取：持有它本身就是持有锁，丢弃它即释放。
    #[allow(dead_code)]
    lock: std::sync::MutexGuard<'static, ()>,
}

/// 获取 `tokens.toml` 写锁，存活到本作用域结束。
pub fn lock_token_store() -> TokenStoreGuard {
    // 某个临界区里 panic 不得把后续所有写锁死：这把锁保护的是一份每次都从
    // 磁盘重读的文件，被污染的中间状态没有任何值得保留的不变量。
    let lock = TOKEN_STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    TokenStoreGuard { lock }
}

/// 打开共享 state SQLite 文件上的配额用量存储。
fn open_usage_store(state_dir: &Path) -> Result<TokenUsageStore> {
    TokenUsageStore::open(&state_dir.join("tokens.db")).context("failed to open token usage store")
}

/// 由 CLI 传入（`--window`/`--quota-*`）的、已解析的配额窗口项。
#[derive(Debug, Clone)]
pub struct NewQuotaWindow {
    pub window: String,
    pub quota_mode: String,
    pub quota_limit: i64,
}

/// Arguments for `token create`.
#[derive(Clone)]
pub struct CreateArgs {
    pub name: String,
    pub models: Vec<String>,
    pub windows: Vec<NewQuotaWindow>,
    pub expires: Option<String>,
    /// : If non-empty, `windows` are placed into a single
    /// `model_quota_groups` entry for these models instead of flat
    /// `quota_windows`.
    pub group_models: Vec<String>,
}

/// Validate `CreateArgs` (shared by locked/unlocked entry points).
fn validate_create_args(args: &CreateArgs) -> Result<()> {
    if args.name.trim().is_empty() {
        bail!("token name cannot be empty");
    }
    validate_windows(&args.windows)?;
    if let Some(ref exp) = args.expires {
        chrono::DateTime::parse_from_rfc3339(exp).with_context(|| {
            format!("invalid --expires '{exp}', expect ISO 8601 (e.g. 2026-12-31T23:59:59Z)")
        })?;
    }
    Ok(())
}

/// 把一个新 token 插入 `config`（生成 key + 唯一性校验）。
fn create_in(config: &mut TokensConfig, args: &CreateArgs) -> Result<TokenConfig> {
    let mut key = generate_key();
    let mut tries = 0;
    while config.tokens.iter().any(|t| t.key == key) {
        tries += 1;
        if tries > 10 {
            bail!("could not generate a unique token key after 10 attempts");
        }
        key = generate_key();
    }
    if config.tokens.iter().any(|t| t.name == args.name) {
        bail!("a token named '{}' already exists", args.name);
    }
    let token = TokenConfig {
        key: key.clone(),
        name: args.name.clone(),
        allowed_models: args.models.clone(),
        quota_windows: if args.group_models.is_empty() {
            args.windows
                .iter()
                .map(|w| QuotaWindow {
                    window: w.window.clone(),
                    quota_mode: w.quota_mode.clone(),
                    quota_limit: w.quota_limit,
                })
                .collect()
        } else {
            vec![]
        },
        model_quota_groups: if args.group_models.is_empty() {
            vec![]
        } else {
            vec![ModelQuotaGroup {
                models: args.group_models.clone(),
                quota_windows: args
                    .windows
                    .iter()
                    .map(|w| QuotaWindow {
                        window: w.window.clone(),
                        quota_mode: w.quota_mode.clone(),
                        quota_limit: w.quota_limit,
                    })
                    .collect(),
            }]
        },
        expired_at: args.expires.clone(),
        bad_request_budget_daily_limit: None,
        bad_request_budget_lifetime_limit: None,
        admin: false,
    };
    config.tokens.push(token.clone());
    Ok(token)
}

/// 创建 token：生成唯一 key、追加进 `tokens.toml`，并返回创建结果。
///
/// 调用方在整段操作期间持有 `tokens.toml` 写锁：CLI 本地写在
/// ownership flock 之内再取这把锁；server handler 在执行变更前取锁，并一路持有
/// 到内存 reload 之后。
pub fn create_token(
    guard: &TokenStoreGuard,
    config_path: &Path,
    args: CreateArgs,
) -> Result<TokenConfig> {
    validate_create_args(&args)?;
    mutate_tokens(guard, config_path, |config| {
        Ok(Some(create_in(config, &args)?))
    })
}

/// 按 key 从 `config` 中移除 token。
fn delete_in(config: &mut TokensConfig, key: &str) -> Result<()> {
    let before = config.tokens.len();
    config.tokens.retain(|t| t.key != key);
    if config.tokens.len() == before {
        bail!("token disappeared concurrently");
    }
    Ok(())
}

/// 为已删除的 token 清理配额运行时数据行（§12.4）。
///
/// 错误必须向上传播：`tokens.toml` 条目此时已经消失且无法回滚，吞掉清理失败
/// 会留下无人认领的行，并把一次"只做了一半的删除"报告成成功。
fn cleanup_usage(state_dir: &Path, label: &str, key: &str) -> Result<()> {
    let store = open_usage_store(state_dir)?;
    store
        .delete_token_usage(key)
        .with_context(|| format!("failed to delete quota rows for token '{label}'"))
}

/// 从 tokens.toml 移除 token，并删除其窗口用量行（§12.4）。
/// 参数既可传完整 key，也可传 token 名称。
///
/// 两半都在调用方的 [`TokenStoreGuard`] 之下完成，因此"删配置条目"与"清配额行"
/// 是同一个临界区：并发的管理写不可能在这两步之间把条目覆盖回来，让我们清掉一个
/// 其实还活着的 token 的行。
pub fn delete_token(
    guard: &TokenStoreGuard,
    config_path: &Path,
    state_dir: &Path,
    key_or_name: &str,
) -> Result<()> {
    let config = TokensConfig::load(&tokens_path(config_path))?;
    let token = find_token(&config, key_or_name)?;
    let name = token.name.clone();
    let key = token.key.clone();
    let key_for_cleanup = key.clone();

    mutate_tokens(guard, config_path, move |cfg| {
        delete_in(cfg, &key)?;
        Ok(Some(()))
    })?;

    if let Err(err) = cleanup_usage(state_dir, &name, &key_for_cleanup) {
        anyhow::bail!(
            "token '{name}' was removed from tokens.toml, but its quota rows could not be \
             deleted: {err}. The token is already inactive; the rows are dropped at the next \
             server start or token reload."
        );
    }
    Ok(())
}

/// 重命名某个 token（按 key 或名称指定原 token，传入新名称）。
///
/// 写入在 [`TokenStoreGuard`] 写锁内完成，并同步更新状态目录下的 `cooldowns.json`
/// 中的 `token` 显示名称与 `token_key_hash`。
pub fn rename_token(
    guard: &TokenStoreGuard,
    config_path: &Path,
    state_dir: &Path,
    key_or_name: &str,
    new_name: &str,
) -> Result<()> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        bail!("token name cannot be empty");
    }

    let config = TokensConfig::load(&tokens_path(config_path))?;
    let token = find_token(&config, key_or_name)?;
    let old_name = token.name.clone();
    let token_key = token.key.clone();

    if old_name == new_name {
        return Ok(());
    }

    if config.tokens.iter().any(|t| t.name == new_name) {
        bail!("a token named '{new_name}' already exists");
    }

    let key_to_update = token_key.clone();
    let name_to_set = new_name.to_string();
    mutate_tokens(guard, config_path, move |cfg| {
        let t = cfg
            .tokens
            .iter_mut()
            .find(|t| t.key == key_to_update)
            .context("token disappeared during rename")?;
        t.name = name_to_set.clone();
        Ok(Some(()))
    })?;

    // 同步更新 state_dir 下 cooldowns.json 中的 token 显示名称与 hash
    let cooldown_path = state_dir.join("cooldowns.json");
    let _ = crate::cooldown::rename_token_in_file(&cooldown_path, &old_name, new_name, &token_key);

    Ok(())
}

/// 重置某个 token 的全部用量计数（按 key 或名称）。
pub fn reset_usage(config_path: &Path, state_dir: &Path, key_or_name: &str) -> Result<()> {
    let config = TokensConfig::load(&tokens_path(config_path))?;
    let token = find_token(&config, key_or_name)?;
    let store = open_usage_store(state_dir)?;
    store.reset_token_usage(&token.key)
}

/// 读取某个 token 当前的窗口用量，用于展示。
pub fn usage(
    config_path: &Path,
    state_dir: &Path,
    key_or_name: &str,
) -> Result<(TokenConfig, Vec<WindowUsage>)> {
    let config = TokensConfig::load(&tokens_path(config_path))?;
    let token = find_token(&config, key_or_name)?;
    let store = open_usage_store(state_dir)?;
    // 先确保窗口行存在，刚创建的 token 才能显示出一份清零的基线。
    let _ = store.init_token_windows(&token.key, &token.quota_windows);
    let usages = store.get_token_usage(&token.key)?;
    Ok((token.clone(), usages))
}

/// 列出全部 token（不含用量；列表按 token 逐条走 `usage_rows`）。
pub fn list(config_path: &Path, state_dir: &Path) -> Result<Vec<(TokenConfig, Vec<WindowUsage>)>> {
    let config = TokensConfig::load(&tokens_path(config_path))?;
    let store = open_usage_store(state_dir)?;
    let mut out = Vec::new();
    for token in config.tokens {
        let _ = store.init_token_windows(&token.key, &token.quota_windows);
        let usages = store.get_token_usage(&token.key).unwrap_or_default();
        out.push((token, usages));
    }
    Ok(out)
}

fn find_token<'a>(config: &'a TokensConfig, key_or_name: &str) -> Result<&'a TokenConfig> {
    config
        .tokens
        .iter()
        .find(|t| t.key == key_or_name || t.name == key_or_name)
        .with_context(|| format!("no token matches key or name '{key_or_name}'"))
}

fn validate_windows(windows: &[NewQuotaWindow]) -> Result<()> {
    for w in windows {
        super::config::parse_duration(&w.window)
            .with_context(|| format!("invalid --window '{}'", w.window))?;
        if w.quota_limit <= 0 {
            bail!("--quota-limit must be positive");
        }
    }
    Ok(())
}

/// 在写锁内变更 `tokens.toml`，写入前后各做一次冲突检测（spec §5.1）。
///
/// 闭包收到解析后的配置并返回一个可选载荷。成功时新配置被序列化并原子写入。
/// 若初始读取与写入之间磁盘上的文件已变（并发的**人工**编辑），则把盘上版本
/// 保留为 `tokens.toml.bak` 并中止写入，交给调用方对齐。
///
///  [`TokenStoreGuard`] 参数才是让"检查 + 写入"在本进程内不可被穿过的东西：
/// 在它之前，两个并发的 server handler 可以双双通过写前 hash
/// 检查，后完成者把先完成者的整份文件静默覆盖掉。
pub fn mutate_tokens<T>(
    _guard: &TokenStoreGuard,
    config_path: &Path,
    f: impl FnOnce(&mut TokensConfig) -> Result<Option<T>>,
) -> Result<T>
where
    T: Send + 'static,
{
    let path = tokens_path(config_path);
    mutate_tokens_inner(&path, f)
}

/// [`mutate_tokens`] 的内部实现：带冲突检测的读-改-写。互斥由调用方持有的
/// guard 提供，本函数只保证"检查与写入不可被本进程的其它写者穿过"。
fn mutate_tokens_inner<T>(
    path: &Path,
    f: impl FnOnce(&mut TokensConfig) -> Result<Option<T>>,
) -> Result<T>
where
    T: Send + 'static,
{
    let original_bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let original_hash = hash_bytes(&original_bytes);

    let mut config: TokensConfig = if original_bytes.is_empty() {
        TokensConfig::default()
    } else {
        let content = std::str::from_utf8(&original_bytes)
            .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
        TokensConfig::parse(content).with_context(|| format!("{} is invalid", path.display()))?
    };

    let payload = f(&mut config)?;
    let payload = payload.ok_or_else(|| anyhow::anyhow!("token operation produced no result"))?;

    // 冲突检测（第一道，乐观）：在我们初次读取之后，文件是否被别的写者动过
    // ——例如用户在期间手工编辑了 tokens.toml。
    let current_hash = match std::fs::read(path) {
        Ok(bytes) => hash_bytes(&bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => hash_bytes(&[]),
        Err(err) => return Err(err.into()),
    };
    if current_hash != original_hash {
        // 中止前先保住盘上那份（手工编辑的）版本。
        let _ = std::fs::copy(path, path.with_extension("toml.bak"));
        bail!(
            "tokens.toml was modified externally during this operation; aborted write. \
             The on-disk version was preserved as {}. Re-run the command.",
            path.with_extension("toml.bak").display()
        );
    }

    let serialized = toml::to_string_pretty(&serialize_view(&config))
        .context("failed to serialize tokens.toml")?;
    write_atomic(path, serialized.as_bytes())?;

    // 测试专用故障注入：写后复核要能确定性地覆盖"我们 rename 之后另一个写者
    // 立刻 rename 上来"这条路，而这个时机在生产代码里没有可供测试挂钩的点。
    #[cfg(test)]
    tests::clobber_if_armed(path);

    // 写后复核。上面那道 hash 检查是乐观的，只挡得住发生在它
    // **之前**的写者；一把不属于本进程的写者仍可以在那个窗口里 rename 到我们
    // 头上。写完再读一次，就把"静默丢失更新"——包括被撤销的 token 复活——
    // 变成一次上报的失败；也正是它保证调用方不会在一个其实没能留在盘上的删除
    // 之后，去清一个仍然有效的 token 的配额行。
    match std::fs::read(path) {
        Ok(bytes) if bytes != serialized.as_bytes() => bail!(
            "tokens.toml was overwritten immediately after this write; the result of this \
             operation is not on disk. Re-run it against the current file."
        ),
        Ok(_) => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to re-read {}", path.display()));
        }
    }

    Ok(payload)
}

/// 只序列化由本模块管理的那部分字段。`TokensConfig` 派生了 Deserialize 但没有
/// Serialize，所以这里手工搭一份 TOML 文档视图。
fn serialize_view(config: &TokensConfig) -> toml::Value {
    use toml::Table;
    use toml::Value;

    let mut root = Table::new();

    // defaults / model_ratios / model_prefix_ratios 由别处重读原始文件来保住；
    // 这里只管 `[[tokens]]` 加上透传字段。
    let mut defaults = Table::new();
    defaults.insert(
        "model_ratio".into(),
        Value::Float(config.defaults.model_ratio),
    );
    defaults.insert(
        "completion_ratio".into(),
        Value::Float(config.defaults.completion_ratio),
    );
    root.insert("defaults".into(), Value::Table(defaults));

    if !config.model_ratios.is_empty() {
        let mut t = Table::new();
        for (k, v) in &config.model_ratios {
            t.insert(k.clone(), Value::Float(*v));
        }
        root.insert("model_ratios".into(), Value::Table(t));
    }
    if !config.model_prefix_ratios.is_empty() {
        let mut t = Table::new();
        for (k, v) in &config.model_prefix_ratios {
            t.insert(k.clone(), Value::Float(*v));
        }
        root.insert("model_prefix_ratios".into(), Value::Table(t));
    }

    let tokens: Vec<Value> = config
        .tokens
        .iter()
        .map(|t| {
            let mut tbl = Table::new();
            tbl.insert("key".into(), Value::String(t.key.clone()));
            tbl.insert("name".into(), Value::String(t.name.clone()));
            tbl.insert(
                "allowed_models".into(),
                Value::Array(
                    t.allowed_models
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            if let Some(ref exp) = t.expired_at {
                tbl.insert("expired_at".into(), Value::String(exp.clone()));
            }
            let windows: Vec<Value> = t
                .quota_windows
                .iter()
                .map(|w| {
                    let mut wt = Table::new();
                    wt.insert("window".into(), Value::String(w.window.clone()));
                    wt.insert("quota_mode".into(), Value::String(w.quota_mode.clone()));
                    wt.insert("quota_limit".into(), Value::Integer(w.quota_limit));
                    Value::Table(wt)
                })
                .collect();
            tbl.insert("quota_windows".into(), Value::Array(windows));
            Value::Table(tbl)
        })
        .collect();
    root.insert("tokens".into(), Value::Array(tokens));
    Value::Table(root)
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 原子写入：临时文件 + rename，读者永远不会看到写了一半的文件。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;

    // Restrict permissions to owner-only (spec §11.1) before publishing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// : Migrate tokens.toml to tokens/ directory.
///
/// Reads the legacy tokens.toml file, creates a tokens/ directory,
/// writes each token as a separate .toml file, and renames the old
/// file to tokens.toml.migrated.
pub fn migrate_to_directory(config_path: &Path) -> Result<()> {
    let tokens_path = tokens_path(config_path);
    let tokens_dir = tokens_path.parent().unwrap().join("tokens");

    if !tokens_path.exists() {
        bail!(
            "tokens.toml not found at {}; nothing to migrate",
            tokens_path.display()
        );
    }

    if tokens_dir.exists() {
        bail!(
            "tokens/ directory already exists at {}; refusing to overwrite",
            tokens_dir.display()
        );
    }

    // Read the legacy config
    let content = std::fs::read_to_string(&tokens_path)
        .with_context(|| format!("failed to read {}", tokens_path.display()))?;
    let config: TokensConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", tokens_path.display()))?;

    // Create the tokens/ directory
    std::fs::create_dir_all(&tokens_dir)
        .with_context(|| format!("failed to create directory {}", tokens_dir.display()))?;

    // Write each token as a separate file
    for token in &config.tokens {
        let filename = format!("{}.toml", token.name);
        let file_path = tokens_dir.join(&filename);
        let token_toml = toml::to_string_pretty(token)
            .with_context(|| format!("failed to serialize token '{}'", token.name))?;
        std::fs::write(&file_path, token_toml)
            .with_context(|| format!("failed to write {}", file_path.display()))?;
    }

    // Rename the old file
    let backup_path = tokens_path.with_extension("toml.migrated");
    std::fs::rename(&tokens_path, &backup_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tokens_path.display(),
            backup_path.display()
        )
    })?;

    println!(
        "Migrated {} token(s) from {} to {}/",
        config.tokens.len(),
        tokens_path.display(),
        tokens_dir.display()
    );
    println!("Old file renamed to {}", backup_path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cfg_path(dir: &Path) -> PathBuf {
        dir.join("config.toml")
    }

    /// 两个写者的会合点。本仓 std 的 `Barrier` 没有带超时的等待接口，所以用原子量
    /// 自己搭一个两方屏障：等到第二个写者到达，或者等满超时——修复之后第二个写者
    /// 根本进不来，超时正是预期路径。
    #[derive(Default)]
    struct Rendezvous {
        arrived: std::sync::atomic::AtomicUsize,
        go: std::sync::atomic::AtomicBool,
    }

    impl Rendezvous {
        fn meet(&self, timeout: std::time::Duration) {
            use std::sync::atomic::Ordering::SeqCst;
            self.arrived.fetch_add(1, SeqCst);
            let deadline = std::time::Instant::now() + timeout;
            while !self.go.load(SeqCst) && std::time::Instant::now() < deadline {
                if self.arrived.load(SeqCst) >= 2 {
                    self.go.store(true, SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// 给下一次写入挂一个"外来覆盖"，用于确定性验证写后复核（一次性）。
    pub(super) static CLOBBER_AFTER_WRITE: std::sync::Mutex<Option<(PathBuf, Vec<u8>)>> =
        std::sync::Mutex::new(None);

    /// 由 [`mutate_tokens_inner`] 在原子写之后、写后复核之前调用。
    ///
    /// 槽是进程级共享的，而测试并行执行：只在注入目标就是当前文件时才消费。
    /// 无条件 `take()` 会让恰好路过写路径的其它测试把注入吃掉——实测表现为
    /// 误吃方在 `assert_eq` 上 panic、arm 方 `expect_err` 落空，两测同红。
    /// arm 错路径的测试自身仍会红（注入不触发，`expect_err` 落空），不依赖
    /// `assert_eq` 也守得住。
    pub(super) fn clobber_if_armed(path: &Path) {
        let mut slot = CLOBBER_AFTER_WRITE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(slot.as_ref(), Some((target, _)) if target == path) {
            return;
        }
        let (target, bytes) = slot.take().expect("armed clobber targets this path");
        std::fs::write(&target, bytes).expect("arm clobber write");
    }

    /// 进程外的写者穿过我们的写前检查时，丢失更新必须**上报**，
    /// 不得静默当作成功。去掉写后复核本测试即失败。
    #[test]
    fn lost_update_after_write_is_reported() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let path = tokens_path(&cp);
        let seeded = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "seed".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        );
        assert!(seeded.is_ok());
        let foreign = std::fs::read(&path).unwrap();

        // 下一次写入落盘后，假装另一个写者用"不含本次新增"的旧内容盖掉我们。
        *CLOBBER_AFTER_WRITE.lock().unwrap() = Some((path.clone(), foreign.clone()));
        let err = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "will-be-clobbered".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .expect_err("被外来写者覆盖必须报错，不能报成功");
        assert!(
            err.to_string().contains("overwritten immediately"),
            "got: {err}"
        );
        let names: Vec<String> = TokensConfig::load(&path)
            .unwrap()
            .tokens
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(
            !names.iter().any(|n| n == "will-be-clobbered"),
            "盘上应当是外来写者的内容：{names:?}"
        );
    }

    /// 生产调用方在整段写入期间持有 store guard；测试同样持有，才会走到与
    /// 线上一致的临界区。
    fn create_with_store_guard(config_path: &Path, args: CreateArgs) -> Result<TokenConfig> {
        let guard = lock_token_store();
        create_token(&guard, config_path, args)
    }

    fn delete_with_store_guard(
        config_path: &Path,
        state_dir: &Path,
        key_or_name: &str,
    ) -> Result<()> {
        let guard = lock_token_store();
        delete_token(&guard, config_path, state_dir, key_or_name)
    }

    /// 回归基线：两个并发写不得静默丢失更新。
    ///
    /// 同步屏障放在**变更闭包里**——闭包正好落在"首次读之后、写前 hash 检查
    /// 之前"，即竞争窗口内部。去掉 `TOKEN_STORE_LOCK` 后两个线程都会在闭包里
    /// 会合、各自基于同一份旧快照通过检查、后写者整份覆盖前写者，最终只剩一个
    /// token；带锁时第二个线程只能等第一个整段写完才开始读，因此屏障必然超时
    /// （超时是预期路径，不是失败），两个 token 并存。
    #[test]
    fn concurrent_creates_do_not_lose_an_update() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let barrier = std::sync::Arc::new(Rendezvous::default());

        std::thread::scope(|s| {
            let handles: Vec<_> = ["race-a", "race-b"]
                .iter()
                .map(|name| {
                    let cp = cp.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    let name = (*name).to_string();
                    s.spawn(move || {
                        let args = CreateArgs {
                            name,
                            models: vec![],
                            windows: vec![],
                            expires: None,
                            group_models: vec![],
                        };
                        let guard = lock_token_store();
                        mutate_tokens(&guard, &cp, move |cfg| {
                            barrier.meet(std::time::Duration::from_millis(400));
                            Ok(Some(create_in(cfg, &args)?))
                        })
                        .expect("concurrent create must succeed");
                    })
                })
                .collect();
            for handle in handles {
                handle.join().expect("writer thread panicked");
            }
        });

        let names: Vec<String> = TokensConfig::load(&tokens_path(&cp))
            .unwrap()
            .tokens
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(
            names.contains(&"race-a".to_string()) && names.contains(&"race-b".to_string()),
            "两个并发 create 都必须留在盘上，实际得到: {names:?}"
        );
    }

    /// 回归基线（fail-open 方向）：`token delete` 撞上并发 `create` 时，
    /// 撤销不得被静默撤销。
    ///
    /// 去锁时赢方（create）的快照是"删除之前"读到的列表 ⇒ 被撤销的 token 回到
    /// `tokens.toml` 继续有效，而 delete 一侧还会照常 reload 并清掉它的配额行，
    /// 净效果是"活着的 token 额度被清零"。这里同时断言两半：撤销生效、且存活的
    /// 那个 create 没被覆盖回去。
    #[test]
    fn concurrent_delete_is_not_silently_revived() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let victim = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "victim".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();

        let barrier = std::sync::Arc::new(Rendezvous::default());
        let victim_key = victim.key.clone();

        std::thread::scope(|s| {
            // 删除方
            let deleter = {
                let cp = cp.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                s.spawn(move || {
                    let guard = lock_token_store();
                    mutate_tokens(&guard, &cp, move |cfg| {
                        barrier.meet(std::time::Duration::from_millis(400));
                        delete_in(cfg, &victim_key)?;
                        Ok(Some(()))
                    })
                    .expect("delete must succeed");
                })
            };
            // 并发新增方（赢方候选：它的快照里带着 victim）
            let creator = {
                let cp = cp.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                s.spawn(move || {
                    let args = CreateArgs {
                        name: "late-comer".to_string(),
                        models: vec![],
                        windows: vec![],
                        expires: None,
                        group_models: vec![],
                    };
                    let guard = lock_token_store();
                    mutate_tokens(&guard, &cp, move |cfg| {
                        barrier.meet(std::time::Duration::from_millis(400));
                        Ok(Some(create_in(cfg, &args)?))
                    })
                    .expect("concurrent create must succeed");
                })
            };
            deleter.join().expect("deleter thread panicked");
            creator.join().expect("creator thread panicked");
        });

        let tokens = TokensConfig::load(&tokens_path(&cp)).unwrap().tokens;
        assert!(
            !tokens.iter().any(|t| t.key == victim.key),
            "被撤销的 token 不得因并发覆盖回到配置里（fail-open 撤销失效）： {:?}",
            tokens.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(
            tokens.iter().any(|t| t.name == "late-comer"),
            "另一侧的 create 也不得丢失： {:?}",
            tokens.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn create_and_read_back() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "alice".to_string(),
                models: vec!["gpt-4o".to_string()],
                windows: vec![NewQuotaWindow {
                    window: "5h".to_string(),
                    quota_mode: "requests".to_string(),
                    quota_limit: 60,
                }],
                expires: Some("2026-12-31T23:59:59Z".to_string()),
                group_models: vec![],
            },
        )
        .unwrap();

        assert!(token.key.starts_with("lp-"));
        assert_eq!(token.name, "alice");

        let reloaded = TokensConfig::load(&tokens_path(&cp)).unwrap();
        assert_eq!(reloaded.tokens.len(), 1);
        assert_eq!(reloaded.tokens[0].allowed_models, vec!["gpt-4o"]);
        assert_eq!(reloaded.tokens[0].quota_windows.len(), 1);
        assert_eq!(reloaded.tokens[0].quota_windows[0].quota_limit, 60);
    }

    #[test]
    fn duplicate_name_rejected() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let mk = || CreateArgs {
            name: "bob".to_string(),
            models: vec![],
            windows: vec![],
            expires: None,
            group_models: vec![],
        };
        create_with_store_guard(&cp, mk()).unwrap();
        assert!(create_with_store_guard(&cp, mk()).is_err());
    }

    #[test]
    fn delete_removes_token() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "carol".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();
        delete_with_store_guard(&cp, dir.path(), &token.key).unwrap();
        let reloaded = TokensConfig::load(&tokens_path(&cp)).unwrap();
        assert!(reloaded.tokens.is_empty());
    }

    /// 定级实测：清理成功时库里一行不剩，于是"重建 token
    /// 继承旧用量"这条路被物理切断；`create_with_store_guard` 每次新生成随机 key，
    /// 同名重建也不会撞上旧行。
    #[test]
    fn delete_removes_usage_rows_so_no_recreated_token_can_inherit_them() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let window = QuotaWindow {
            window: "1h".to_string(),
            quota_mode: "requests".to_string(),
            quota_limit: 10,
        };
        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "gina".to_string(),
                models: vec![],
                windows: vec![NewQuotaWindow {
                    window: window.window.clone(),
                    quota_mode: window.quota_mode.clone(),
                    quota_limit: window.quota_limit,
                }],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();
        {
            let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
            store
                .reserve_request_slot(&token.key, std::slice::from_ref(&window))
                .unwrap();
            assert_eq!(store.get_token_usage(&token.key).unwrap()[0].used, 1);
        }

        delete_with_store_guard(&cp, dir.path(), &token.key).unwrap();

        let store = TokenUsageStore::open(&dir.path().join("tokens.db")).unwrap();
        assert!(
            store.get_token_usage(&token.key).unwrap().is_empty(),
            "删除成功的 token 不得留下配额行"
        );
    }

    /// 回归基线：清理配额行失败时必须上报，并如实说明"配置已删、行还在"。
    /// 修复前该失败被 `if let Ok(store)` 与 `let _ =` 双重吞掉 —— 命令报成功，
    /// 幽灵配额行留在库里，操作员无从得知。
    #[test]
    fn delete_reports_quota_row_cleanup_failure() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "frank".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();
        // 让配额库不可打开：目标路径本身是个目录。
        std::fs::create_dir(dir.path().join("tokens.db")).unwrap();

        let err = delete_with_store_guard(&cp, dir.path(), &token.key)
            .expect_err("cleanup failure must not be swallowed");
        let msg = err.to_string();
        assert!(msg.contains("removed from tokens.toml"), "got: {msg}");
        assert!(msg.contains("could not be deleted"), "got: {msg}");
        assert!(
            !msg.contains(&token.key),
            "错误信息不得回显 token 明文：{msg}"
        );
        assert!(
            TokensConfig::load(&tokens_path(&cp))
                .unwrap()
                .tokens
                .is_empty(),
            "前半段（配置删除）确实已生效"
        );
    }

    #[test]
    fn invalid_expires_rejected() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let result = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "dave".to_string(),
                models: vec![],
                windows: vec![],
                expires: Some("not-a-date".to_string()),
                group_models: vec![],
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn empty_models_persist_and_reload() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        create_with_store_guard(
            &cp,
            CreateArgs {
                name: "eve".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();
        let reloaded = TokensConfig::load(&tokens_path(&cp)).unwrap();
        assert!(reloaded.tokens[0].allowed_models.is_empty());
    }

    /// `f64a125` 修复的第二个问题：CLI 本地写路径由
    /// `with_cli_write_lock_or_delegate` 的外层框架持有 ownership.lock，被它
    /// 调用的 `*_unlocked` 必须**不再取锁**。同进程以不同 fd 再次请求排他锁
    /// 在 flock 层必然失败（按 fd 计数），当年 E2E 报出的错误是"持有者是自己"。
    ///
    /// 覆盖边界（如实说明）：两个 `*_unlocked` 都不接受 state_dir，因此本测试
    /// 只证明它们在"给定目录的锁已被持有"时仍能完成写入。若有人把它们改成从
    /// 全局 `service::state_dir()` 重新取锁，本测试不会失败 —— 那一层的回归需要
    /// 组合级测试（`with_cli_write_lock_or_delegate_at` 的本地写回调）才能守住。
    #[test]
    fn unlocked_writes_do_not_reacquire_the_ownership_lock() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let lock =
            crate::ownership::OwnershipLock::try_acquire(dir.path(), "cli", "llm-proxy test")
                .expect("acquire ownership lock");

        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "while-locked".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .expect("create_with_store_guard must not re-acquire the ownership lock");
        assert_eq!(token.name, "while-locked");

        delete_with_store_guard(&cp, dir.path(), "while-locked")
            .expect("delete_with_store_guard must not re-acquire the ownership lock");
        let reloaded = TokensConfig::load(&tokens_path(&cp)).unwrap();
        assert!(reloaded.tokens.is_empty());

        drop(lock);
    }

    #[test]
    fn rename_token_updates_tokens_toml_and_cooldown_file() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let state_dir = dir.path();

        let token = create_with_store_guard(
            &cp,
            CreateArgs {
                name: "alice".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();

        // Write a cooldown entry for alice
        let cooldown_path = state_dir.join("cooldowns.json");
        let hash = crate::token::keygen::token_key_hash(&token.key);
        let entry = crate::cooldown::CooldownEntry {
            model: "m1".into(),
            provider: "p1".into(),
            protocol: crate::config::Protocol::OpenaiChatCompletions,
            token: Some("alice".into()),
            token_key_hash: Some(hash.clone()),
            kind: "client_error".into(),
            reason: "400".into(),
            started_at_unix: 1,
            expires_at_unix: 9999999999,
            hit_count: 0,
            last_hit_unix: None,
        };
        crate::cooldown::write_entries(&cooldown_path, vec![entry]).unwrap();

        // Rename alice -> alice-v2
        let guard = lock_token_store();
        rename_token(&guard, &cp, state_dir, "alice", "alice-v2").unwrap();

        // Check tokens.toml
        let reloaded = TokensConfig::load(&tokens_path(&cp)).unwrap();
        assert_eq!(reloaded.tokens.len(), 1);
        assert_eq!(reloaded.tokens[0].name, "alice-v2");
        assert_eq!(reloaded.tokens[0].key, token.key);

        // Check cooldowns.json
        let reloaded_cooldowns = crate::cooldown::read_entries(&cooldown_path).unwrap();
        assert_eq!(reloaded_cooldowns.len(), 1);
        assert_eq!(reloaded_cooldowns[0].token.as_deref(), Some("alice-v2"));
        assert_eq!(
            reloaded_cooldowns[0].token_key_hash.as_deref(),
            Some(hash.as_str())
        );
    }

    #[test]
    fn rename_token_validation_and_conflict_checks() {
        let dir = tempdir().unwrap();
        let cp = cfg_path(dir.path());
        let state_dir = dir.path();

        create_with_store_guard(
            &cp,
            CreateArgs {
                name: "alice".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();

        create_with_store_guard(
            &cp,
            CreateArgs {
                name: "bob".to_string(),
                models: vec![],
                windows: vec![],
                expires: None,
                group_models: vec![],
            },
        )
        .unwrap();

        let guard = lock_token_store();

        // Empty name
        assert!(rename_token(&guard, &cp, state_dir, "alice", "   ").is_err());

        // Duplicate name
        assert!(rename_token(&guard, &cp, state_dir, "alice", "bob").is_err());

        // Non-existent token
        assert!(rename_token(&guard, &cp, state_dir, "charlie", "charlie-v2").is_err());

        // Same name is a no-op
        assert!(rename_token(&guard, &cp, state_dir, "alice", "alice").is_ok());
    }
}
