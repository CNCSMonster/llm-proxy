//! 并发正确性断言。
//!
//! 打在现状真实入口上：
//!   - `get_token_valid` 的"读-判-刷-写"事务 → [`refresh_account_with_urls`]（refresh.rs：
//!     全程持 `data.lock`：锁内 load → HTTP 刷新 → `save_oauth_accounts_locked`）；
//!   - `peek_token`（无锁读）→ [`load_oauth_accounts`]（storage.rs：直接读文件、不取锁）。
//!
//! 断言四条：
//!   ① 无 lost update——并发刷新后落盘 token 序号 == 上游签发总数（锁内 RMW ⇒ 写序==签发序）；
//!   ② 慢 refresh 期间 peek **全程不被阻塞且每次读到可完整解析的已提交快照（不校验新鲜度）**；
//!   ③ 备份数 ≤ save 次数+初始（只断上界；另断保留上界 3，存在性按 `backup_path_for` ns 规则）；
//!   ④ 无 `.tmp` 残留。
//!
//! 失败处置：**失败即修 storage，不得固化绕过**。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::refresh::refresh_account_with_urls;
use super::storage::{find_backups_newest_first, load_oauth_accounts, save_oauth_accounts};
use super::types::{AccountsFile, OpenaiAccount, unix_secs};

// ── 测试上游：慢刷新 + 按签发顺序编号的 token（序号可比大小，供 lost-update 断言）──

#[derive(Clone)]
struct TokenSvc {
    hits: Arc<AtomicU64>,
    sleep_ms: u64,
}

async fn token_endpoint(
    axum::extract::State(svc): axum::extract::State<TokenSvc>,
) -> axum::Json<serde_json::Value> {
    tokio::time::sleep(Duration::from_millis(svc.sleep_ms)).await;
    let n = svc.hits.fetch_add(1, Ordering::SeqCst) + 1;
    axum::Json(serde_json::json!({
        // 序号即签发顺序；长度 ≥20 且 access≠refresh（过 storage 校验）
        "access_token": format!("upstream-serial-{:020}", n),
        "expires_in": 0u64, // 恒过期 → 每次调用全量刷新（强制竞争），且不满足 60s skip
    }))
}

pub(super) async fn spawn_refresh_server(sleep_ms: u64) -> (std::net::SocketAddr, Arc<AtomicU64>) {
    let svc = TokenSvc {
        hits: Arc::new(AtomicU64::new(0)),
        sleep_ms,
    };
    let hits = svc.hits.clone();
    let app = axum::Router::new()
        .route("/token", axum::routing::post(token_endpoint))
        .with_state(svc);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (addr, hits)
}

// ── 固件：强制过期、校验全过的账号 ──

fn fixture_accounts(now: i64) -> AccountsFile {
    let mut accounts = AccountsFile::new();

    accounts.openai.insert(
        "work".to_string(),
        OpenaiAccount {
            account_label: "p0a@example.com".to_string(),
            access_token: "initial-access-token-000000000002".to_string(),
            refresh_token: "initial-refresh-token-000000000002".to_string(),
            expires_at_unix: now - 3600,
            updated_at_unix: now - 7200,
        },
    );
    accounts
}

/// 在独立 std 线程上经 `Handle::block_on` 驱动刷新——线程自身不占 runtime worker，
/// 避免 `acquire_lock` 的同步自旋占用调度线程导致本地 testserver 饿死（deadlock-free 结构）。
fn spawn_refresh_thread(
    handle: tokio::runtime::Handle,
    path: std::path::PathBuf,
    account: &'static str,
    kind: &'static str,
    url: String,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        handle.block_on(async move {
            refresh_account_with_urls(&path, account, kind, &url)
                .await
                .expect("refresh");
        })
    })
}

/// 等待 `data.lock` 被慢刷新持有（自己取锁失败 = 持有者在手），供 peek 时序确定性。
fn wait_until_lock_held(path: &Path) -> bool {
    use fs2::FileExt;
    let lock_path = path.parent().unwrap_or(Path::new(".")).join("data.lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // 只探测锁，不改锁文件内容
        .open(&lock_path)
        .expect("open lock file");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if file.try_lock_exclusive().is_err() {
            return true; // 被刷新线程持有
        }
        let _ = file.unlock();
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_no_tmp_residue(dir: &Path) {
    let residue: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(residue.is_empty(), "④ .tmp 残留: {residue:?}");
}

/// ①③④：多线程并发刷新（同一把全局 `data.lock` 上的锁内 RMW）→ 无 lost update、
/// 备份与临时文件不变量保持。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_loses_no_update_and_keeps_storage_invariants() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");

    let (oai_addr, oai_hits) = spawn_refresh_server(250).await;
    let oai_url = format!("http://{oai_addr}/token");

    // 初始落盘（file 不存在 → 不产生备份，作为"初始"计数基准）
    save_oauth_accounts(&path, &fixture_accounts(unix_secs() as i64)).expect("write fixture");

    // 3 线程并发刷同一 openai 账号（同类型同锁竞争）
    let handle = tokio::runtime::Handle::current();
    let mut workers = Vec::new();
    for _ in 0..3 {
        workers.push(spawn_refresh_thread(
            handle.clone(),
            path.clone(),
            "work",
            "openai_oauth",
            oai_url.clone(),
        ));
    }
    for w in workers {
        // 最坏持锁等待 = 5 × 250ms ≈ 1.25s < acquire_lock 5s 超时（超时即红，属"失败即修"范围）
        w.join().expect("refresh thread panicked");
    }

    // 上游恰好各签发 3 次（恒过期 → 无 60s skip；线程全 Ok → 无失败静默）
    assert_eq!(oai_hits.load(Ordering::SeqCst), 3, "openai 上游请求数");

    // ① 无 lost update：各自字段落盘 == 各自上游签发总数（锁内 RMW ⇒ 后写必为最新签发）
    let (accounts, _skipped) = load_oauth_accounts(&path).expect("final read");

    let oai = accounts.openai.get("work").expect("openai acct");
    assert_eq!(
        oai.access_token,
        format!("upstream-serial-{:020}", 3),
        "① openai 字段 lost update"
    );

    // ③ 备份上界：save（file 已存在）6 次 + 初始 1 次；另符合保留上界 3
    let backups = find_backups_newest_first(&path);
    assert!(
        backups.len() <= 6 + 1,
        "③ 备份数 {} 超过 save 次数+初始",
        backups.len()
    );
    assert!(
        backups.len() <= 3,
        "③ 备份保留上界 3，实为 {}",
        backups.len()
    );

    // ④ 无 .tmp 残留
    assert_no_tmp_residue(temp.path());
}

/// ②：慢 refresh 全程持锁期间，peek（无锁读）全程不被阻塞，且每次读到可完整解析的
/// 已提交快照（不校验新鲜度）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_reads_never_block_during_slow_refresh() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    save_oauth_accounts(&path, &fixture_accounts(unix_secs() as i64)).expect("write fixture");

    // 单独的慢速上游（600ms）：每次刷新持锁 ≈600ms
    let (addr, hits) = spawn_refresh_server(600).await;
    let url = format!("http://{addr}/token");

    let handle = tokio::runtime::Handle::current();
    let refresh_thread = spawn_refresh_thread(handle, path.clone(), "work", "openai_oauth", url);

    // 确认慢刷新已持有 data.lock 后再开始 peek（时序确定性）
    assert!(
        wait_until_lock_held(&path),
        "慢 refresh 未在 2s 内取得锁——测试前提不成立"
    );

    let peek_start = Instant::now();
    let mut worst = Duration::ZERO;
    for i in 0..20 {
        let t0 = Instant::now();
        // 读到的必须是可完整解析的已提交快照（不校验新鲜度）
        let _parsed = load_oauth_accounts(&path).unwrap_or_else(|err| {
            panic!(
                "② 第 {i} 次 peek 读到不可解析的快照（{}）: {err:#}",
                path.display()
            )
        });
        let elapsed = t0.elapsed();
        worst = worst.max(elapsed);
        assert!(
            elapsed < Duration::from_millis(300),
            "② 第 {i} 次 peek 被阻塞 {elapsed:?}（阈值 300ms，慢刷新持锁 600ms）"
        );
    }
    assert!(
        peek_start.elapsed() < Duration::from_millis(600),
        "② 20 次 peek 应远短于剩余持锁窗口"
    );
    eprintln!("② peek worst={worst:?}");

    refresh_thread.join().expect("refresh thread panicked");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "刷新应恰好发生 1 次");

    // 刷新确实完成并原子发布（读到最新已提交快照）
    let (accounts, _) = load_oauth_accounts(&path).expect("final read");
    assert_eq!(
        accounts.openai.get("work").expect("work").access_token,
        format!("upstream-serial-{:020}", 1)
    );

    // ④ 收尾无 .tmp 残留
    assert_no_tmp_residue(temp.path());
}
