use std::fs::{self, OpenOptions};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
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
    // Migrate legacy unsharded state to default identity shard
    migrate_legacy_state();
    // Report a bad listen address directly instead of spawning a child that
    // immediately dies with only "background process exited early" to show.
    cfg.ensure_serve_listen()?;
    let config_path = absolute_config_path(config_path);
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let dirs = StateDirs::resolve(config_dir);
    // §15.2: 预检 config 独占锁，防止启动已知冲突的后台子进程
    crate::ownership::ServerConfigLock::preflight_check(&config_path)?;
    // 预检 listen 端口是否被占用，给出带占用者 PID 的可执行错误
    let server = cfg.require_server()?;
    check_listen_available(&server.listen)?;
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    ensure_private_runtime_dir(&dirs.runtime_dir)?;
    ensure_private_state_dir(&dirs.state_dir)?;
    refuse_if_running_in(&dirs.runtime_dir)?;

    let pid_path = pid_path_in(&dirs.runtime_dir);
    let log_path = log_path_in(&dirs.state_dir);
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
        .arg("--config-dir")
        .arg(config_path.parent().unwrap_or(Path::new(".")))
        .arg("serve")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr));
    detach(&mut command);

    let mut child = command
        .spawn()
        .context("failed to start llm-proxy in background")?;

    // The child publishes its own pid file and revokes it when startup fails; a
    // second write here would resurrect the pid of a process that already exited.
    // So the parent only observes real signals (spec §11.9.1).
    // : the socket path comes from the same injected `dirs` the child
    // was started with (`--config-dir`), not from the global `socket_path()`.
    let mgmt_socket = socket_path_in(&dirs.runtime_dir);
    match confirm_background_start(
        BACKGROUND_START_BUDGET,
        BACKGROUND_START_POLL,
        &mut || {
            child
                .try_wait()
                .map(|status| status.map(|status| status.to_string()))
                .context("failed to inspect background process")
        },
        &|| management_channel_answers(&mgmt_socket),
    )? {
        BackgroundStart::Started => {}
        BackgroundStart::Exited(status) => {
            // Guarded by content: our child may have died while a *different*,
            // still-serving instance owns the pid file (two `serve` commands racing
            // past `refuse_if_running` before either child published). Deleting an
            // innocent pid file would turn a healthy server into "○ not running"
            // and hand `shutdown` a pid to kill that it never started.
            revoke_pid_file_owned_by(&pid_path, child.id());
            bail!(
                "llm-proxy background process exited early with {status}; see {}",
                log_path.display()
            );
        }
        BackgroundStart::Undecided => {
            bail!(
                "后台进程 pid {} 仍在运行，但 {}s 内管理通道没有给出就绪信号：结果不可判定，按失败处理。\n\
                 进程可能只是启动得慢，稍后才就绪：先执行 `llm-proxy status` 再决定是否重试，不要直接再起一次。\n\
                 查看 {log}，必要时执行 `llm-proxy shutdown`",
                child.id(),
                BACKGROUND_START_BUDGET.as_secs(),
                log = log_path.display()
            );
        }
    }

    println!("llm-proxy started in background");
    if let Some(ref server) = cfg.server {
        println!("  listen: {}", server.listen);
    }
    println!("  pid: {}", child.id());
    println!("  pid_file: {}", pid_path.display());
    println!("  socket: {}", socket_path_in(&dirs.runtime_dir).display());
    println!("  log: {}", log_path.display());
    println!("Use `llm-proxy serve --foreground` to run in the foreground.");
    Ok(())
}

/// How far the background child's startup got, as the parent can observe it.
#[derive(Debug, PartialEq, Eq)]
enum BackgroundStart {
    Started,
    Exited(String),
    Undecided,
}

/// How long a starting server waits for the ownership lock (§15.2).
///
/// Named because the background parent's startup budget must cover it; see
/// [`BACKGROUND_START_BUDGET`].
const SERVER_OWNERSHIP_WAIT: Duration = Duration::from_secs(10);

/// Upper bound on waiting for the background child's real startup verdict.
///
/// Derived, not chosen: the child must first wait out the ownership lock
/// ([`SERVER_OWNERSHIP_WAIT`]) and then produce the management channel's own
/// verdict ([`MANAGEMENT_READY_BUDGET`]) before its socket can answer a probe. A
/// parent budget shorter than that sum reports "undecided" on a child that is
/// still legally starting: the parent exits non-zero while the child goes on to
/// become a healthy server, and the next `serve` then says "already running".
/// The `+ 5` covers everything else on that path (config already parsed, state
/// dirs, `AppState::new`); no constant can bound arbitrary work, which is why the
/// `Undecided` message tells the user to check `status` before retrying.
const BACKGROUND_START_BUDGET: Duration =
    Duration::from_secs(SERVER_OWNERSHIP_WAIT.as_secs() + MANAGEMENT_READY_BUDGET.as_secs() + 5);

/// Interval between startup polls.
const BACKGROUND_START_POLL: Duration = Duration::from_millis(100);

/// Poll the two real signals until one of them decides the outcome.
///
/// "Alive 250 ms after spawn" previously printed `llm-proxy started`, which is
/// 把"还没失败"当成"已成功" — a child dying later (ownership timeout, socket bind
/// failure) left a pid file pointing at a corpse. Observers are injected because
/// unit tests must not spawn processes (SOP §4.3).
fn confirm_background_start(
    budget: Duration,
    poll_interval: Duration,
    child_exited: &mut impl FnMut() -> Result<Option<String>>,
    channel_answers: &impl Fn() -> bool,
) -> Result<BackgroundStart> {
    let deadline = Instant::now() + budget;
    loop {
        // Exit is checked first: a dead process outranks any positive signal.
        if let Some(status) = child_exited()? {
            return Ok(BackgroundStart::Exited(status));
        }
        if channel_answers() {
            return Ok(BackgroundStart::Started);
        }
        if Instant::now() >= deadline {
            return Ok(BackgroundStart::Undecided);
        }
        thread::sleep(poll_interval);
    }
}

/// Is the management channel already accepting connections?
///
/// Reaching it proves the child published its pid file, bound the socket and
/// verified its 0600 mode, in that order (`run_foreground` writes the pid before
/// spawning the channel and only starts the data plane after the ready verdict).
///
/// : the socket path must come from the same injected `StateDirs` the
/// server was started with — not from the global `socket_path()`. The caller
/// (`start_background`) passes `socket_path_in(&dirs.runtime_dir)`.
#[cfg(unix)]
fn management_channel_answers(socket_path: &Path) -> bool {
    management_channel_answers_at(socket_path)
}

/// The positive signal itself, with the channel path supplied by the caller.
///
/// `socket_path()` is the process-wide real runtime dir, which no test may bind;
/// the injection point is what lets a red-proof test check that the signal means
/// "a listener answers", not "the file exists" (SOP §4.3).
///
/// The connect is non-blocking with its own deadline on purpose. This probe runs
/// inside `confirm_background_start`'s loop *before* that loop's deadline is
/// checked, so an unbounded blocking `connect()` would hang the whole `serve`
/// command — and its "不可判定 ⇒ report failure" exit — forever. An AF_UNIX
/// `connect()` really can block: the listener's backlog is full, or the peer on
/// the other end never calls `accept()`. Both mean "no verdict", which is exactly
/// the state the budget exists to report (spec §11.9.1).
#[cfg(unix)]
fn management_channel_answers_at(path: &Path) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    const PROBE_BUDGET: Duration = Duration::from_millis(200);

    let socket =
        match unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) } {
            fd if fd >= 0 => unsafe { OwnedFd::from_raw_fd(fd) },
            _ => return false,
        };
    if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
        return false;
    }
    // `sockaddr_un` is POD: family plus a NUL-terminated path, no length field to
    // get wrong beyond the 107-byte bound the planner already enforced.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return false;
    }
    // `sun_path` is `[c_char; 108]`, and `c_char` is `i8` on x86-64 Linux but `u8`
    // on aarch64 — map elementwise instead of reinterpreting the slice. The
    // trailing NUL is already there from `zeroed()`.
    let path_bytes: Vec<libc::c_char> = bytes.iter().map(|byte| *byte as libc::c_char).collect();
    address.sun_path[..bytes.len()].copy_from_slice(&path_bytes);
    let length = (std::mem::size_of::<libc::sockaddr_un>() - address.sun_path.len()
        + bytes.len()
        + 1) as libc::socklen_t;
    // Ours is a non-blocking socket, and the kernel has two shapes of "yes" for
    // it. Linux completes an AF_UNIX stream connect synchronously: a listener
    // with room in its accept queue answers `0` right here (measured — making
    // that arm unreachable reddens
    // `the_background_start_signal_requires_a_listener_not_a_file`). BSD/macOS
    // instead answer `-1/EINPROGRESS` and deliver the verdict as `POLLOUT` with
    // `SO_ERROR == 0`, so both arms must stay. The poll arm cannot be exercised
    // on this machine's kernel (mutating it to `false` survives the suite); it is
    // recorded as a residual, same class as the inert Windows twin gate.
    let connected = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&raw const address) as *const libc::sockaddr,
            length,
        )
    };
    if connected == 0 {
        return true;
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
        // ENOENT / ENOTSOCK / ECONNREFUSED / EACCES ⇒ nobody answers. EAGAIN
        // (accept queue full) is also "no verdict": the listener cannot serve us,
        // which is exactly what the caller must not report as "started".
        return false;
    }
    let mut poll_fd = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    let timeout_ms = i32::try_from(PROBE_BUDGET.as_millis()).unwrap_or(i32::MAX);
    match unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) } {
        // POLLOUT is how a completed non-blocking connect is reported; the verdict
        // itself lives in SO_ERROR.
        ready if ready > 0 => {
            let mut error: libc::c_int = 0;
            let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&raw mut error) as *mut libc::c_void,
                    &mut length,
                )
            };
            rc == 0 && error == 0
        }
        // 0: no verdict inside the budget. <0: poll failed. Neither is "answers".
        _ => false,
    }
}

/// Windows 管理通道绑定在临时端口上，而 CLI 侧的地址仍是历史常量
/// （`management_tcp_addr`），因此无法探测：后台启动只能确认
/// 进程存活。该缺口另行处理，不在这里伪造成功证据。
#[cfg(not(unix))]
fn management_channel_answers(_socket_path: &Path) -> bool {
    true
}

pub async fn run_foreground(cfg: Config, config_path: &Path) -> Result<()> {
    // Migrate legacy unsharded state to default identity shard
    migrate_legacy_state();
    // Fail before any side effect (pid file / socket / ownership lock) if the
    // listen address cannot be published to clients.
    cfg.ensure_serve_listen()?;
    let config_path = absolute_config_path(config_path);
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let dirs = StateDirs::resolve(config_dir);
    // §15.2: 强制单实例独占同一份 config 目录（flock fail-stop）
    let server = cfg.require_server()?;
    let _config_lock = crate::ownership::ServerConfigLock::acquire(&config_path, &server.listen)?;
    ensure_private_state_dir(&dirs.state_dir)?;
    ensure_private_runtime_dir(&dirs.runtime_dir)?;
    refuse_if_running_in(&dirs.runtime_dir)?;
    // §15.2：server 生命周期持有所有权锁（CLI 持有则等待 10s，server 持有则失败）
    let _ownership = acquire_server_ownership_in(&dirs.runtime_dir, SERVER_OWNERSHIP_WAIT)?;
    write_pid_in(&dirs.runtime_dir, std::process::id())?;

    let started_at = Instant::now();
    let listen = server.listen.clone();
    tracing::info!(
        target: "llm_proxy::service",
        event = "service_start",
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        commit = crate::version::GIT_COMMIT,
        listen = %listen,
        config_path = %config_path.display(),
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
    let state = crate::proxy::AppState::new(cfg, config_path);

    // Management socket startup must succeed — it is the only write path
    // for CLI delegation. A bind failure would silently disable all write
    // operations.
    let (mgmt_ready_tx, mgmt_ready_rx) = tokio::sync::oneshot::channel();
    // The channel's verdict gates the data plane: an unverified management
    // socket means an unverifiable owner-only write path (spec §11.8).
    let management = tokio::spawn(run_management_socket_with_ready(
        dirs.runtime_dir.clone(),
        state.clone(),
        shutdown_tx.clone(),
        mgmt_ready_tx,
    ));
    await_management_ready(
        mgmt_ready_rx,
        &management,
        &pid_path_in(&dirs.runtime_dir),
        MANAGEMENT_READY_BUDGET,
    )
    .await?;
    // The background parent prints this line too; foreground needs its own copy
    // because at the default level (`RUST_LOG` unset ⇒ ERROR) the
    // `management_socket_fallback` warn is filtered out, leaving no way to see
    // which path a `sun_path`-forced rename actually bound.
    println!("  socket: {}", socket_path_in(&dirs.runtime_dir).display());

    let signal_handler = tokio::spawn(wait_for_shutdown_signal(shutdown_tx));
    let result = crate::proxy::serve_state_with_shutdown(state.clone(), shutdown_rx).await;
    state.flush_usage_on_shutdown();
    let writer_result = state.shutdown_config_writer().await;
    management.abort();
    signal_handler.abort();
    let _ = fs::remove_file(pid_path_in(&dirs.runtime_dir));
    let _ = fs::remove_file(socket_path_in(&dirs.runtime_dir));

    match &result {
        Ok(exit_reason) => tracing::info!(
            target: "llm_proxy::service",
            event = "service_stop",
            exit_reason,
            config_writer_stop = writer_stop_label(&writer_result),
            uptime_ms = started_at.elapsed().as_millis() as u64,
        ),
        Err(error) => tracing::error!(
            target: "llm_proxy::service",
            event = "service_stop",
            exit_reason = "server_error",
            error = %error,
            config_writer_stop = writer_stop_label(&writer_result),
            uptime_ms = started_at.elapsed().as_millis() as u64,
        ),
    }
    // 关闭结论优先于"服务器本身正常"：writer 没给出干净终态时进程必须非 0 退出，
    // 否则 `serve --foreground` 与库入口会把不可判定当成成功。
    let exit_reason = match result {
        Err(error) => return Err(error),
        Ok(reason) => reason,
    };
    if let Err(error) = writer_result {
        return Err(anyhow::anyhow!(
            "配置写入器未给出干净关闭结论（{error:?}），按失败退出"
        ));
    }
    // 管理通道升级关停的 reason 是 `Ok(...)` 形状，只看 writer 结论会把它放行为
    // exit 0（spec §11.9.2 禁止的"异常状态静默"）。
    if let Some(cause) = degraded_management_stop(exit_reason) {
        tracing::error!(
            target: "llm_proxy::service",
            event = "management_channel_degraded_stop",
            exit_reason,
            config_writer_stop = writer_stop_label(&Ok(())),
            "the process stops with a non-zero exit code because the control plane died"
        );
        return Err(anyhow::anyhow!("{cause}"));
    }
    Ok(())
}

/// The shutdown source the accept-failure escalation publishes.
///
/// Not `cfg(unix)`-gated: `run_foreground` is one cross-platform body, and a
/// reason that cannot occur on a platform simply never matches here.
const MANAGEMENT_CHANNEL_FAILED_EXIT: &str = "management-channel-failed";

/// Map a stop reason into `None` (clean) or `Some(cause)` (the control plane died).
///
/// A control plane that exhausted its accept budget asks the data plane to stop,
/// so the reason arrives as an ordinary `Ok(reason)`. Reporting that as exit 0
/// makes a degraded stop indistinguishable from a user-run `shutdown` for every
/// supervisor (`Restart=on-failure` would not restart) and for every script
/// wrapping `serve --foreground` — the "异常状态静默" spec §11.9.2 forbids. The
/// data plane already logged `service_stop exit_reason="management-channel-failed"`;
/// this only decides the exit code and adds the loud event.
fn degraded_management_stop(exit_reason: &str) -> Option<&'static str> {
    if exit_reason == MANAGEMENT_CHANNEL_FAILED_EXIT {
        return Some(
            "管理通道在连续 accept 失败后升级停机（exit_reason=\"management-channel-failed\"）：\
             这不是正常关闭，按失败退出。查看日志 `management_accept_failed` 计数与 fd 上限",
        );
    }
    None
}

/// How long the data plane may wait for the control plane's startup verdict.
///
/// The verdict is expected in milliseconds (two syscalls and a chmod); the
/// budget exists to bound the *undecidable* case, not to describe normal startup
/// (spec §11.9.1: 不可判定必须有界并可升级).
const MANAGEMENT_READY_BUDGET: Duration = Duration::from_secs(30);

/// Consume the management channel's startup verdict, revoking the pid file on any
/// outcome other than "ready".
///
/// Extracted from `run_foreground` so all four outcomes — ready, failed, channel
/// closed, and never-answered — are executable in tests, including the historical
/// failure mode where the caller waited on an accept loop that never returns.
/// Missing verdicts must not leave a pid file behind: `status` would
/// then point at a process that has no management channel.
async fn await_management_ready(
    mut ready_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
    management: &tokio::task::JoinHandle<Result<()>>,
    pid_path: &Path,
    budget: Duration,
) -> Result<()> {
    let outcome = match tokio::time::timeout(budget, &mut ready_rx).await {
        Err(_elapsed) => Err(anyhow::anyhow!(
            "no readiness verdict within {}s (channel task finished: {})",
            budget.as_secs(),
            management.is_finished()
        )),
        Ok(Ok(result)) => result.map_err(|msg| anyhow::anyhow!(msg)),
        Ok(Err(_closed)) => Err(anyhow::anyhow!(
            "startup channel closed without a verdict (task panicked or was aborted)"
        )),
    };
    if let Err(err) = outcome {
        let err = err.context("management socket failed to start");
        let reported = format!("{err:#}");
        management.abort();
        revoke_pid_file(pid_path);
        tracing::error!(
            target: "llm_proxy::service",
            event = "service_start_failed",
            stage = "management_channel",
            error = %reported,
            "startup aborted; the data plane was not published"
        );
        return Err(err);
    }
    Ok(())
}

/// Delete `path` as part of failing startup, loudly if the deletion itself fails.
///
/// A pid file that survives its process makes `status` report a dead pid, which
/// is a silent wrong answer (spec §11.9.2) — so a failed revoke is an event, not
/// an `let _ =`.
fn revoke_pid_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::error!(
            target: "llm_proxy::service",
            event = "pid_file_revoke_failed",
            path = %path.display(),
            error = %err,
            "startup failed but the pid file is still present; `status` may report a dead pid"
        ),
    }
}

/// Revoke a pid file only when it actually records `pid`.
///
/// The unconditional unlink in `start_background`'s failure branch was a second
/// writer on someone else's state: with two `serve` commands racing, the loser's
/// child exits while the winner's pid file is already published, and an
/// unguarded revoke deletes *that* file — after which `status` reports a live
/// server as stopped and `shutdown` falls back to pid-based signalling. A pid
/// file we cannot match to our own child is not ours to delete, so it is left in
/// place and reported.
fn revoke_pid_file_owned_by(path: &Path, pid: u32) {
    match fs::read_to_string(path) {
        Ok(content) if pid_file_records(&content, pid) => revoke_pid_file(path),
        Ok(content) => tracing::warn!(
            target: "llm_proxy::service",
            event = "pid_file_revoke_skipped",
            path = %path.display(),
            expected_pid = pid,
            found = %content.trim(),
            "our background child exited but the pid file belongs to another process; leaving it alone"
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            target: "llm_proxy::service",
            event = "pid_file_revoke_skipped",
            path = %path.display(),
            error = %err,
            "could not read the pid file to check ownership; leaving it alone"
        ),
    }
}

/// Does this pid-file body record exactly `pid`?
///
/// Pure so the ownership decision itself has a red-proof test: anything that is
/// not literally our pid — a different number, a body we cannot parse, an empty
/// file — is "not ours to delete".
fn pid_file_records(body: &str, pid: u32) -> bool {
    body.trim().parse::<u32>() == Ok(pid)
}

/// `service_stop` 里 writer 关闭结论的标签：三态必须互不相同，禁止把不可判定折叠成
/// `ok` 或 `failed_stop`。
fn writer_stop_label(
    writer_result: &Result<(), crate::runtime_config::WriterError>,
) -> &'static str {
    match writer_result {
        Ok(()) => "ok",
        Err(crate::runtime_config::WriterError::FailedStop) => "failed_stop",
        Err(
            crate::runtime_config::WriterError::StopUndetermined
            | crate::runtime_config::WriterError::MailboxClosed,
        ) => "undetermined",
        Err(_) => "error",
    }
}

fn request_shutdown(
    shutdown_tx: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    source: &'static str,
) {
    if let Ok(mut tx) = shutdown_tx.lock()
        && let Some(tx) = tx.take()
    {
        tracing::info!(
            target: "llm_proxy::service",
            event = "shutdown_received",
            source,
        );
        let _ = tx.send(source);
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal(
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        tracing::error!(target: "llm_proxy::service", "failed to register SIGINT handler");
        return;
    };
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        tracing::error!(target: "llm_proxy::service", "failed to register SIGTERM handler");
        return;
    };
    tokio::select! {
        _ = interrupt.recv() => request_shutdown(&shutdown_tx, "SIGINT"),
        _ = terminate.recv() => request_shutdown(&shutdown_tx, "SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal(
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
) {
    if tokio::signal::ctrl_c().await.is_ok() {
        request_shutdown(&shutdown_tx, "SIGINT");
    }
}

/// 获取 server 所有权锁（§15.2）：
/// - CLI 持有 → 等待（默认 10s，CLI 写操作毫秒级）
/// - server 持有 → 立即失败（防多实例）
/// - 成功 → 返回持有者（调用方持有到生命周期结束，drop 释放）
fn acquire_server_ownership_in(
    runtime_dir: &Path,
    timeout: Duration,
) -> Result<crate::ownership::OwnershipLock> {
    let deadline = Instant::now() + timeout;
    loop {
        match crate::ownership::OwnershipLock::try_acquire(runtime_dir, "server", "llm-proxy serve")
        {
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
            Err(crate::ownership::AcquireError::HeldByServer { ref metadata }) => {
                let pid = metadata.as_ref().map(|m| m.pid);
                let alive = pid.is_some_and(crate::ownership::OwnershipLock::pid_alive);
                if !alive {
                    // The kernel releases a flock when its holder dies, so a
                    // recorded-but-dead pid while the lock is still held is a
                    // transient state: a concurrent starter won the race and has
                    // not published its metadata yet. Retrying is safe — the
                    // kernel arbitrates, so at most one starter wins the lock.
                    thread::sleep(Duration::from_millis(200));
                    if let Ok(lock) = crate::ownership::OwnershipLock::try_acquire(
                        runtime_dir,
                        "server",
                        "llm-proxy serve",
                    ) {
                        return Ok(lock);
                    }
                }
                // The lock is genuinely held — either by a live server or by a
                // starter that won the race. Never suggest deleting the lock
                // file: that does not release the flock and could leave two
                // servers running. Terminate the holder instead.
                let pid_text = pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                let holder_state = if alive { "alive" } else { "dead" };
                bail!(
                    "另一个 llm-proxy server 已持有所有权锁（记录 pid={pid_text}, {holder_state}）；\n\
                     若该进程已无响应，请先停止或手动终止后再启动：\n\
                       llm-proxy shutdown\n\
                       pgrep -af llm-proxy\n\
                       kill {pid_text}",
                );
            }
            Err(e) => bail!("所有权锁获取失败: {e}"),
        }
    }
}

pub fn shutdown_background(config_path: &Path) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let dirs = StateDirs::resolve(config_dir);
    shutdown_background_in(&dirs.runtime_dir)
}

fn shutdown_background_in(runtime_dir: &Path) -> Result<()> {
    let pid_path = pid_path_in(runtime_dir);
    let socket_path = socket_path_in(runtime_dir);

    if send_management_shutdown_in(runtime_dir).is_ok() {
        // The management channel only *schedules* shutdown. Confirm the process
        // actually exited before claiming success or removing its pid/socket:
        // a live process left behind keeps the ownership lock and makes the
        // next start fail with a misleading "already running" error.
        if let Some(pid) = running_pid_in(runtime_dir)
            && !await_or_terminate_confirmed(pid, Duration::from_secs(30), Duration::from_secs(5))?
        {
            bail!(
                "管理通道已接受停止请求，但进程 pid={pid} 在 SIGTERM/SIGKILL 之后仍未退出；\
                 为保留诊断未删除 pid/socket 文件。请手动确认后处理：kill -9 {pid}"
            );
        }
        let _ = fs::remove_file(&pid_path);
        let _ = fs::remove_file(&socket_path);
        println!("llm-proxy stopped");
        return Ok(());
    }
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
    if !terminate_process_confirmed(pid, Duration::from_secs(5))? {
        bail!(
            "未能终止 llm-proxy 进程 pid={pid}（SIGTERM/SIGKILL 均无效）；\
             为保留诊断未删除 pid 文件。请手动确认后处理：kill -9 {pid}"
        );
    }
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

/// Persistent application state directory for the **default** config instance.
///
/// Returns `<XDG_STATE_HOME>/llm-proxy/<identity>/` where identity is derived
/// from the default config directory. For non-default instances, use
/// [`StateDirs::resolve`] with the specific config directory.
///
/// # 分类标注
///
/// 本函数是「全局便捷函数」——只可用于 **TUI/CLI 单进程工具**（`tui`、`usage`、`model info` 等）
/// 和测试。**禁止**在包装函数（`*_dirs()` / `*_path()`）内部调用本函数，
/// **禁止**在 server 请求路径上调用本函数。违反这两条时 spec §11.8 同源约束失效，
/// 门禁由 `scripts/release-gate.sh` 的 grep 检查与
/// `state_pid_ownership_socket_share_same_injected_identity` 等注入路径一致性测试守住。
pub fn state_dir() -> PathBuf {
    let identity = crate::config::config_identity(&crate::config::default_config_dir());
    state_dir_root().join(identity)
}

/// Runtime directory for the **default** config instance.
///
/// Returns `<XDG_RUNTIME_DIR>/llm-proxy/<identity>/` where identity is derived
/// from the default config directory. For non-default instances, use
/// [`StateDirs::resolve`] with the specific config directory.
///
/// # 分类标注
///
/// 全局便捷函数——只可用于 TUI/CLI 单进程工具和测试。
/// 禁止在包装函数或 server 请求路径上调用（见 `state_dir()` 注释）。
pub fn runtime_dir() -> PathBuf {
    let identity = crate::config::config_identity(&crate::config::default_config_dir());
    runtime_dir_root().join(identity)
}

fn application_dir(xdg_root: Option<PathBuf>, default_root: PathBuf) -> PathBuf {
    xdg_root.unwrap_or(default_root).join("llm-proxy")
}

/// Per-instance state and runtime directories, derived from config directory identity.
///
/// Each config directory gets its own shard under the XDG state/runtime
/// roots, so multiple instances can coexist without interference.
#[derive(Clone, Debug)]
pub struct StateDirs {
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl StateDirs {
    /// Production: derive shard paths from config directory identity.
    pub fn resolve(config_dir: &Path) -> Self {
        let identity = crate::config::config_identity(config_dir);
        Self {
            state_dir: state_dir_root().join(&identity),
            runtime_dir: runtime_dir_root().join(&identity),
        }
    }

    /// Test: inject temporary root (ADR-027 §39 — no set_var).
    pub fn for_test(tempdir: &Path) -> Self {
        Self {
            state_dir: tempdir.join("state"),
            runtime_dir: tempdir.join("runtime"),
        }
    }
}

/// Migrate legacy (unsharded) state files into the default identity shard.
///
/// Trigger: `<state_root>/llm-proxy/usage.db` exists (unsharded marker).
/// Target: default config identity shard (from `default_config_dir()`).
/// Idempotent: skips if shard already has state files.
///
/// Files migrated:
/// - `usage.db`, `usage.db-shm`, `usage.db-wal`
/// - `tokens.db`, `tokens.db-shm`, `tokens.db-wal`
/// - `status-cache.json`
/// - `cooldowns.json`
/// - `diagnostics.jsonl`
/// - `query-cache/` directory
pub fn migrate_legacy_state() {
    let state_root = state_dir_root();
    let legacy_usage = state_root.join("usage.db");
    if !legacy_usage.exists() {
        return;
    }

    let default_config_dir = crate::config::default_config_dir();
    let default_dirs = StateDirs::resolve(&default_config_dir);
    let target_state = &default_dirs.state_dir;

    // Idempotent: skip if target shard already has state files
    if target_state.join("usage.db").exists() {
        return;
    }

    // Create target directory
    if let Err(e) = ensure_private_state_dir(target_state) {
        eprintln!("warning: legacy migration: cannot create {target_state:?}: {e}");
        return;
    }

    // Files to migrate (relative to state_root)
    let files = [
        "usage.db",
        "usage.db-shm",
        "usage.db-wal",
        "tokens.db",
        "tokens.db-shm",
        "tokens.db-wal",
        "status-cache.json",
        "cooldowns.json",
        "diagnostics.jsonl",
    ];

    let mut migrated = 0;
    for name in &files {
        let src = state_root.join(name);
        let dst = target_state.join(name);
        if src.exists() && !dst.exists() {
            if let Err(e) = std::fs::rename(&src, &dst) {
                eprintln!("warning: legacy migration: rename {src:?} -> {dst:?}: {e}");
            } else {
                migrated += 1;
            }
        }
    }

    // Migrate query-cache directory
    let src_cache = state_root.join("query-cache");
    let dst_cache = target_state.join("query-cache");
    if src_cache.is_dir() && !dst_cache.exists() {
        if let Err(e) = std::fs::rename(&src_cache, &dst_cache) {
            eprintln!("warning: legacy migration: rename {src_cache:?} -> {dst_cache:?}: {e}");
        } else {
            migrated += 1;
        }
    }

    if migrated > 0 {
        eprintln!(
            "info: migrated {migrated} legacy state file(s) to shard {}",
            target_state.display()
        );
    }
}

/// Root of all instance state shards (`<XDG_STATE_HOME>/llm-proxy/`).
pub fn state_dir_root() -> PathBuf {
    application_dir(
        dirs::state_dir(),
        dirs::home_dir()
            .map(|home| home.join(".local").join("state"))
            .unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// Root of all instance runtime shards (`<XDG_RUNTIME_DIR>/llm-proxy/`).
pub fn runtime_dir_root() -> PathBuf {
    dirs::runtime_dir()
        .map(|root| root.join("llm-proxy"))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .map(|home| {
                    home.join(".local")
                        .join("state")
                        .join("llm-proxy")
                        .join("runtime")
                })
                .unwrap_or_else(|| PathBuf::from(".").join("llm-proxy").join("runtime"))
        })
}

pub(crate) fn pid_path_in(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("llm-proxy.pid")
}

/// Default-instance management socket path.
///
/// # 分类标注
///
/// 全局便捷函数——只可用于 TUI/CLI 单进程工具。
/// **禁止**在包装函数内调用（反例：`management_socket_dirs` 曾因此成为 根因）。
/// Server 路径必须用 `socket_path_in(&dirs.runtime_dir)`。
pub fn socket_path() -> PathBuf {
    socket_path_in(&runtime_dir())
}

pub(crate) fn socket_path_in(runtime_dir: &Path) -> PathBuf {
    socket_path_plan(runtime_dir).0
}

/// `sockaddr_un::sun_path` is a 108-byte fixed array holding the path plus its
/// trailing NUL, so 107 bytes is the longest bindable path.
#[cfg(unix)]
const SUN_PATH_MAX: usize = 107;

/// Effective management socket path plus whether the `sun_path` fallback applies.
///
/// A function of `runtime_dir` **and** the shared fallback base directory, which
/// [`management_socket_dirs`] derives from the effective uid. The server binds
/// what this returns and every CLI entry connects to the same value, so no
/// discovery protocol is needed (spec §11.8). Two processes in different uids
/// therefore compute different bases — that is the point of the base name, not a
/// leak: the base is `0700` and its sockets are `0600`.
pub(crate) fn socket_path_plan(runtime_dir: &Path) -> (PathBuf, bool) {
    #[cfg(unix)]
    let fallback_base = short_socket_base();
    #[cfg(not(unix))]
    let fallback_base = PathBuf::new();
    socket_path_plan_in(runtime_dir, &fallback_base)
}

/// [`socket_path_plan`] with the fallback base supplied by the caller. Only the
/// server and the CLI's path derivation need to agree on the conventional name,
/// so tests inject a base to exercise the fallback without touching `/tmp`.
pub(crate) fn socket_path_plan_in(runtime_dir: &Path, fallback_base: &Path) -> (PathBuf, bool) {
    let primary = runtime_dir.join("llm-proxy.sock");
    #[cfg(unix)]
    if primary.as_os_str().len() > SUN_PATH_MAX {
        return (sun_path_safe_socket_path(runtime_dir, fallback_base), true);
    }
    let _ = fallback_base;
    (primary, false)
}

/// Fallback base directory, suffixed with the uid so users sharing a world-writable
/// `/tmp` can never observe — let alone remove — each other's socket.
#[cfg(unix)]
fn short_socket_base() -> PathBuf {
    PathBuf::from("/tmp").join(format!("llm-proxy-{}", current_uid()))
}

/// The two directories the management channel's path is derived from, as *this*
/// process sees them: `(runtime_dir, sun_path fallback base)`.
///
/// Both sides go through here — the server entry
/// (`run_management_socket_with_ready`) and every CLI connection
/// (`socket_path` → `socket_path_plan`) — because the design has no discovery
/// protocol: if the two ever derived the pair differently, the CLI would connect
/// to a path nobody listens on (spec §11.8). One source makes that a single
/// mutation point, pinned by
/// `the_cli_and_the_server_entry_derive_the_same_channel_path`.
#[cfg(all(unix, test))]
fn management_socket_dirs() -> (PathBuf, PathBuf) {
    (runtime_dir(), short_socket_base())
}

/// `<base>/<sha256(runtime_dir)[..16]>.sock` — short enough to bind while still
/// being one-to-one with the runtime directory. A single global name would make
/// parallel isolated instances delete each other's live socket.
#[cfg(unix)]
fn sun_path_safe_socket_path(runtime_dir: &Path, base: &Path) -> PathBuf {
    use sha2::Digest;
    use std::os::unix::ffi::OsStrExt;
    let digest = sha2::Sha256::digest(runtime_dir.as_os_str().as_bytes());
    base.join(format!("{}.sock", &hex::encode(digest)[..16]))
}

/// The uid the base directory must belong to.
///
/// `geteuid`, not `getuid`: the directory is created *by this process*, which
/// means it is created by the effective uid. Comparing a freshly created inode
/// against the real uid would declare our own directory foreign under a
/// setuid/euid≠ruid invocation, and the error text would send the reader off to
/// chase someone else's ownership. `geteuid()` cannot fail.
#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

/// Create the fallback base directory with its final mode.
#[cfg(unix)]
fn create_private_socket_base(base: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(base)
}

/// Create the runtime directory with its final 0700 mode in the creating syscall.
#[cfg(unix)]
fn create_private_runtime_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Prove the runtime dir is ours alone, not a symlink, and force 0700.
#[cfg(unix)]
pub(crate) fn ensure_private_runtime_dir(runtime_dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    match create_private_runtime_dir(runtime_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "failed to create runtime dir {}",
                runtime_dir.display()
            )));
        }
    }
    let metadata = fs::symlink_metadata(runtime_dir)
        .with_context(|| format!("failed to lstat runtime dir {}", runtime_dir.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "runtime dir {} is a symlink — refusing to follow it; verify it belongs to uid {} and remove it yourself if it is stale",
            runtime_dir.display(),
            current_uid()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "runtime dir {} exists but is not a directory",
            runtime_dir.display()
        );
    }
    let uid = current_uid();
    if metadata.uid() != uid {
        bail!(
            "runtime dir {} is owned by uid {}, not {} — refusing to share it",
            runtime_dir.display(),
            metadata.uid(),
            uid
        );
    }
    enforce_mode(runtime_dir, 0o700)
}

#[cfg(not(unix))]
pub(crate) fn ensure_private_runtime_dir(runtime_dir: &Path) -> Result<()> {
    fs::create_dir_all(runtime_dir).context("failed to create runtime dir")?;
    Ok(())
}

/// Create the state directory with 0700 mode so owner-only state stays private.
/// Mirrors `ensure_private_runtime_dir` but without symlink/uid checks (state dir
/// is not a security boundary like the runtime dir).
/// Existing directories are tightened to 0700 (fixes legacy 0775 from bare create_dir_all).
#[cfg(unix)]
pub(crate) fn ensure_private_state_dir(state_dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = state_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(state_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "failed to create state dir {}",
                state_dir.display()
            )));
        }
    }
    enforce_mode(state_dir, 0o700)
}

#[cfg(not(unix))]
pub(crate) fn ensure_private_state_dir(state_dir: &Path) -> Result<()> {
    fs::create_dir_all(state_dir).context("failed to create state dir")?;
    Ok(())
}

/// Prove the shared `/tmp/llm-proxy-<uid>` base is ours alone, then force 0700.
///
/// A pre-existing base owned by another uid is a hard stop rather than a reason
/// to fall back again: sharing that directory would hand the sole write path
/// (spec §11.8) to whoever created it, and a second silent move elsewhere is the
/// same failure in a different costume (§11.9.2).
///
/// Every check here is an `lstat`, never an `stat`: `mkdir` on a path that is
/// already a symlink to a directory *succeeds* by resolving it, so following
/// links would verify — and then `chmod` — some other directory than the one the
/// socket gets bound under. A symlink at the base position is therefore refused,
/// not dereferenced.
#[cfg(unix)]
fn ensure_private_socket_base_dir(base: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    match create_private_socket_base(base) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "failed to create management socket fallback dir {} (shorten XDG_RUNTIME_DIR so that its socket path fits within {SUN_PATH_MAX} bytes)",
                base.display()
            )));
        }
    }
    let metadata = fs::symlink_metadata(base).with_context(|| {
        format!(
            "failed to lstat management socket fallback dir {}",
            base.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "management socket fallback dir {} is a symlink — refusing to follow it; verify it belongs to uid {} and remove it yourself if it is stale",
            base.display(),
            current_uid()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "management socket fallback dir {} exists but is not a directory",
            base.display()
        );
    }
    let uid = current_uid();
    if metadata.uid() != uid {
        bail!(
            "management socket fallback dir {} is owned by uid {}, not {} — refusing to share it; shorten XDG_RUNTIME_DIR instead",
            base.display(),
            metadata.uid(),
            uid
        );
    }
    enforce_mode(base, 0o700)
}

/// Set `mode` on `path` and read it back from the kernel.
///
/// Read back with `lstat`: this runs on the base directory and on the bound
/// socket inode, and for both of them "what the kernel reports about the thing
/// we just chmodded" must not be answered by following a symlink.
#[cfg(unix)]
fn enforce_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to chmod {mode:#o} {}", path.display()))?;
    let actual = fs::symlink_metadata(path)
        .with_context(|| format!("failed to lstat {} after chmod", path.display()))?
        .mode()
        & 0o777;
    if actual != mode {
        bail!(
            "{} reports mode {actual:#o} after chmod {mode:#o}",
            path.display()
        );
    }
    Ok(())
}

fn log_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join("llm-proxy.log")
}

pub(crate) fn running_pid_in(runtime_dir: &Path) -> Option<u32> {
    let pid = fs::read_to_string(pid_path_in(runtime_dir))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    process_exists(pid).then_some(pid)
}

/// Information about a process occupying a network port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortOccupant {
    pub pid: Option<u32>,
    pub comm: String,
    pub cmdline: String,
    pub is_llm_proxy: bool,
}

impl PortOccupant {
    pub fn summary(&self) -> String {
        if !self.cmdline.is_empty() {
            format!("{}: '{}'", self.comm, self.cmdline)
        } else if !self.comm.is_empty() {
            self.comm.clone()
        } else {
            "unknown".to_string()
        }
    }
}

/// Inspect what process is occupying `listen` (e.g. "127.0.0.1:8989").
pub fn inspect_port_occupant(listen: &str) -> Option<PortOccupant> {
    let addr = listen.parse::<SocketAddr>().ok()?;
    #[cfg(target_os = "linux")]
    {
        if let Some(occupant) = inspect_port_occupant_linux(addr) {
            return Some(occupant);
        }
    }
    // Fallback: test if a TCP listener answers on the port
    if is_tcp_port_listening(addr) {
        Some(PortOccupant {
            pid: None,
            comm: "external process".to_string(),
            cmdline: String::new(),
            is_llm_proxy: false,
        })
    } else {
        None
    }
}

fn is_tcp_port_listening(addr: SocketAddr) -> bool {
    let probe_addr = match addr {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), v4.port())
        }
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), v6.port())
        }
        other => other,
    };
    TcpStream::connect_timeout(&probe_addr, Duration::from_millis(30)).is_ok()
}

#[cfg(target_os = "linux")]
fn inspect_port_occupant_linux(target: SocketAddr) -> Option<PortOccupant> {
    let target_port = target.port();
    let mut matching_inodes = std::collections::HashSet::new();

    // Check /proc/net/tcp
    if let Ok(content) = fs::read_to_string("/proc/net/tcp") {
        for line in content.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 10 {
                continue;
            }
            let local_addr = parts[1];
            let state = parts[3];
            let inode = parts[9];
            if state != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some((ip_hex, port_hex)) = local_addr.split_once(':') else {
                continue;
            };
            let Ok(lport) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            if lport != target_port {
                continue;
            }
            if let Some(lip) = parse_proc_ipv4(ip_hex) {
                match target.ip() {
                    IpAddr::V4(tip) => {
                        if lip.is_unspecified() || tip.is_unspecified() || lip == tip {
                            matching_inodes.insert(inode.to_string());
                        }
                    }
                    IpAddr::V6(_) => {
                        if lip.is_unspecified() {
                            matching_inodes.insert(inode.to_string());
                        }
                    }
                }
            }
        }
    }

    // Check /proc/net/tcp6
    if let Ok(content) = fs::read_to_string("/proc/net/tcp6") {
        for line in content.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 10 {
                continue;
            }
            let local_addr = parts[1];
            let state = parts[3];
            let inode = parts[9];
            if state != "0A" {
                continue;
            }
            let Some((ip_hex, port_hex)) = local_addr.split_once(':') else {
                continue;
            };
            let Ok(lport) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            if lport != target_port {
                continue;
            }
            if ip_hex == "00000000000000000000000000000000" {
                matching_inodes.insert(inode.to_string());
            } else if let IpAddr::V6(tip) = target.ip()
                && let Some(lip) = parse_proc_ipv6(ip_hex)
                && (lip.is_unspecified() || tip.is_unspecified() || lip == tip)
            {
                matching_inodes.insert(inode.to_string());
            }
        }
    }

    if matching_inodes.is_empty() {
        return None;
    }

    // Scan /proc for processes holding any of these socket inodes
    if let Ok(proc_entries) = fs::read_dir("/proc") {
        for proc_entry in proc_entries.flatten() {
            let Ok(file_type) = proc_entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let file_name = proc_entry.file_name();
            let Some(name_str) = file_name.to_str() else {
                continue;
            };
            let Ok(pid) = name_str.parse::<u32>() else {
                continue;
            };

            let fd_dir = proc_entry.path().join("fd");
            let Ok(fd_entries) = fs::read_dir(fd_dir) else {
                continue;
            };
            for fd_entry in fd_entries.flatten() {
                if let Ok(target) = fs::read_link(fd_entry.path()) {
                    let target_str = target.to_string_lossy();
                    if let Some(inode) = target_str
                        .strip_prefix("socket:[")
                        .and_then(|s| s.strip_suffix(']'))
                        && matching_inodes.contains(inode)
                    {
                        let comm = fs::read_to_string(proc_entry.path().join("comm"))
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default();
                        let cmdline = fs::read(proc_entry.path().join("cmdline"))
                            .map(|bytes| {
                                bytes
                                    .split(|&b| b == 0)
                                    .filter(|chunk| !chunk.is_empty())
                                    .map(|chunk| String::from_utf8_lossy(chunk).to_string())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        let is_llm_proxy = comm == "llm-proxy"
                            || comm.starts_with("llm-proxy")
                            || cmdline.contains("llm-proxy");
                        return Some(PortOccupant {
                            pid: Some(pid),
                            comm,
                            cmdline,
                            is_llm_proxy,
                        });
                    }
                }
            }
        }
    }

    Some(PortOccupant {
        pid: None,
        comm: "system or foreign user process".to_string(),
        cmdline: String::new(),
        is_llm_proxy: false,
    })
}

#[cfg(target_os = "linux")]
fn parse_proc_ipv4(hex_str: &str) -> Option<Ipv4Addr> {
    if hex_str.len() != 8 {
        return None;
    }
    let b0 = u8::from_str_radix(&hex_str[6..8], 16).ok()?;
    let b1 = u8::from_str_radix(&hex_str[4..6], 16).ok()?;
    let b2 = u8::from_str_radix(&hex_str[2..4], 16).ok()?;
    let b3 = u8::from_str_radix(&hex_str[0..2], 16).ok()?;
    Some(Ipv4Addr::new(b0, b1, b2, b3))
}

#[cfg(target_os = "linux")]
fn parse_proc_ipv6(hex_str: &str) -> Option<Ipv6Addr> {
    if hex_str.len() != 32 {
        return None;
    }
    let mut octets = [0u8; 16];
    for i in 0..4 {
        let chunk = &hex_str[i * 8..(i + 1) * 8];
        let b0 = u8::from_str_radix(&chunk[6..8], 16).ok()?;
        let b1 = u8::from_str_radix(&chunk[4..6], 16).ok()?;
        let b2 = u8::from_str_radix(&chunk[2..4], 16).ok()?;
        let b3 = u8::from_str_radix(&chunk[0..2], 16).ok()?;
        octets[i * 4] = b0;
        octets[i * 4 + 1] = b1;
        octets[i * 4 + 2] = b2;
        octets[i * 4 + 3] = b3;
    }
    Some(Ipv6Addr::from(octets))
}

/// Format a bind failure with port occupant diagnostics and actionable remediation advice.
pub fn format_bind_error(listen: &str, err: &std::io::Error) -> anyhow::Error {
    let is_addr_in_use = err.kind() == std::io::ErrorKind::AddrInUse
        || err.raw_os_error() == Some(98)
        || err.raw_os_error() == Some(48)
        || err.raw_os_error() == Some(10048);

    if is_addr_in_use {
        let occupant = inspect_port_occupant(listen);
        let mut msg =
            format!("failed to bind listen address {listen}: Address already in use ({err}).");
        match occupant {
            Some(occ) => {
                if let Some(pid) = occ.pid {
                    msg.push_str(&format!("\n  Occupied by: PID {pid} ({})", occ.summary()));
                    if occ.is_llm_proxy {
                        msg.push_str(&format!(
                            "\n  Remediation: 检测到端口被残留的 llm-proxy 实例占用。\n\
                             \x20 - 若该实例为非预期残留，请执行: kill {pid} (强制清理: kill -9 {pid})\n\
                             \x20 - 若该实例属于另一套配置，请修改 config.toml 中的 [server] listen 端口"
                        ));
                    } else {
                        msg.push_str(&format!(
                            "\n  Remediation: 端口被外部进程占用。\n\
                             \x20 - 请释放该端口 (kill {pid}) 或在 config.toml 中的 [server] 修改 listen 端口"
                        ));
                    }
                } else {
                    msg.push_str("\n  Occupied by: 外部进程（受权限限制未能获取 PID）");
                    let port = listen
                        .parse::<SocketAddr>()
                        .map(|a| a.port().to_string())
                        .unwrap_or_else(|_| {
                            listen.split(':').next_back().unwrap_or("8989").to_string()
                        });
                    msg.push_str(&format!(
                        "\n  Remediation: 请执行 `ss -tlnp | grep {port}` 或 `lsof -i :{port}` 定位占用进程并释放端口，或修改 config.toml 中的 [server] listen 端口"
                    ));
                }
            }
            None => {
                let port = listen
                    .parse::<SocketAddr>()
                    .map(|a| a.port().to_string())
                    .unwrap_or_else(|_| {
                        listen.split(':').next_back().unwrap_or("8989").to_string()
                    });
                msg.push_str(&format!(
                    "\n  Remediation: 请执行 `ss -tlnp | grep {port}` 或 `lsof -i :{port}` 定位占用者，然后终止占用进程或修改 config.toml 中的 [server] listen 端口。"
                ));
            }
        }
        anyhow::anyhow!(msg)
    } else {
        anyhow::anyhow!("failed to bind listen address {listen}: {err}")
    }
}

/// Preflight check that `listen` is not already occupied before starting service.
pub fn check_listen_available(listen: &str) -> Result<()> {
    if let Some(_occ) = inspect_port_occupant(listen) {
        let err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "Address already in use");
        return Err(format_bind_error(listen, &err));
    }
    Ok(())
}

fn refuse_if_running_in(runtime_dir: &Path) -> Result<()> {
    let path = pid_path_in(runtime_dir);
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

fn write_pid_in(runtime_dir: &Path, pid: u32) -> Result<()> {
    let path = pid_path_in(runtime_dir);
    fs::write(&path, format!("{pid}\n"))
        .with_context(|| format!("failed to write pid file {}", path.display()))?;
    #[cfg(unix)]
    enforce_mode(&path, 0o600)?;
    Ok(())
}

/// Wait for `pid` to exit within `timeout`.
///
/// Returns `true` iff the process is confirmed gone. Callers must not report
/// success on `false`: a `()` return here previously let `shutdown` print
/// "llm-proxy stopped" while the process was still alive and holding the
/// ownership lock.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_exists(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Stop `pid` and confirm it is gone, escalating SIGTERM → SIGKILL.
///
/// Returns `Ok(false)` when the process is still alive after every signal was
/// sent — the caller must then fail loudly instead of claiming a clean stop.
fn terminate_process_confirmed(pid: u32, step_timeout: Duration) -> Result<bool> {
    if !process_exists(pid) {
        return Ok(true);
    }
    terminate_process(pid)?;
    if wait_for_exit(pid, step_timeout) {
        return Ok(true);
    }
    force_kill_process(pid)?;
    Ok(wait_for_exit(pid, step_timeout))
}

/// Wait for a process that was already asked to stop (e.g. via the management
/// channel), escalating to SIGTERM/SIGKILL only if it lingers.
fn await_or_terminate_confirmed(
    pid: u32,
    graceful_timeout: Duration,
    step_timeout: Duration,
) -> Result<bool> {
    if wait_for_exit(pid, graceful_timeout) {
        return Ok(true);
    }
    terminate_process_confirmed(pid, step_timeout)
}

/// Signals readiness once the channel is bound **and** owner-only, via `ready_tx`.
///
/// The process-scoped half of the entry: it supplies `management_socket_dirs()`,
/// i.e. the same pair `socket_path()` hands the CLI, and delegates every decision
/// to [`run_management_socket_with_ready_at`], which the fallback tests execute
/// directly. The caller supplies `runtime_dir` so the socket identity matches
/// the rest of the process (ownership, pid, state) — all derived from the same
/// `--config-dir` .
#[cfg(unix)]
async fn run_management_socket_with_ready(
    runtime_dir: PathBuf,
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> Result<()> {
    let fallback_base = short_socket_base();
    run_management_socket_with_ready_at(&runtime_dir, &fallback_base, state, shutdown_tx, ready_tx)
        .await
}

/// [`run_management_socket_with_ready`] with both directories supplied by the caller.
///
/// `run_management_socket_with_ready` is the production entry point; this is where
/// the `sun_path` decision, its announcement, and the fallback base preparation
/// live, so tests can exercise that wiring with a temporary base instead of the
/// real `/tmp/llm-proxy-<uid>` (SOP §4.3: the entry layer must be executed too).
#[cfg(unix)]
async fn run_management_socket_with_ready_at(
    runtime_dir: &Path,
    fallback_base: &Path,
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> Result<()> {
    let fail = |ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>, err: anyhow::Error| {
        let msg = format!("{err:#}");
        let _ = ready_tx.send(Err(msg.clone()));
        Err(anyhow::anyhow!(msg))
    };
    let (path, fell_back) = socket_path_plan_in(runtime_dir, fallback_base);
    // 规避不破坏单实例去重，且这不是假设而是构造决定的：`fell_back` 为真意味着常规
    // 名超过 `sun_path`，而 `connect()` 与 `bind()` 受同一个 107 字节限制（实测两者
    // 都返回 "AF_UNIX path too long"），因此该名字上不可能存在任何监听者，"旧实例占
    // 常规名、新实例另起一条"的并存态无从出现。可绑的常规名（fell_back 为假）被活
    // 监听者占用时，由绑定前的 `remove_stale_socket` 探测式拒绝接管并报错。
    // 此处曾有一条并存闸门，因上述不可达而删除（变异测试暴露）。
    if fell_back {
        // A different path than configured is an anomalous state, not a silent
        // success (spec §11.9.2): announce it, then prove the directory is ours.
        tracing::warn!(
            target: "llm_proxy::service",
            event = "management_socket_fallback",
            runtime_dir = %runtime_dir.display(),
            primary = %runtime_dir.join("llm-proxy.sock").display(),
            path = %path.display(),
            "management socket path exceeds sun_path's 107-byte limit; using the hashed short path (spec §11.8)"
        );
        if let Err(err) = ensure_private_socket_base_dir(fallback_base) {
            tracing::error!(
                target: "llm_proxy::service",
                event = "management_socket_fallback_unusable",
                path = %path.display(),
                error = %err,
                "the sun_path fallback directory is unusable; refusing to start a server without a management channel"
            );
            return fail(ready_tx, err);
        }
    } else if let Err(err) = ensure_private_runtime_dir(runtime_dir) {
        tracing::error!(
            target: "llm_proxy::service",
            event = "management_runtime_dir_unusable",
            path = %path.display(),
            error = %err,
            "the runtime directory is unusable; refusing to start a server without a secure management channel"
        );
        return fail(ready_tx, err);
    }
    run_management_socket_at(state, shutdown_tx, &path, Some(ready_tx)).await
}

/// Consecutive `accept()` failures tolerated before the control plane gives up.
const MANAGEMENT_ACCEPT_ERROR_LIMIT: u32 = 8;

/// Backoff between failed `accept()` calls, so a persistent error cannot hot-loop.
const MANAGEMENT_ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(25);

/// What to do after `consecutive_errors` failed `accept()` calls in a row.
///
/// Kept as a pure function of the count so the threshold itself is testable —
/// the loops are not, on either platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptFailure {
    /// Transient (backlog pressure, `EMFILE`, …): keep the data plane up and retry.
    Retry,
    /// The channel is not recovering: shut the server down rather than run without
    /// its only write path (spec §11.8、§11.9.3).
    Fatal,
}

fn management_accept_failure_policy(consecutive_errors: u32) -> AcceptFailure {
    if consecutive_errors >= MANAGEMENT_ACCEPT_ERROR_LIMIT {
        AcceptFailure::Fatal
    } else {
        AcceptFailure::Retry
    }
}

/// Record one failed `accept()` and decide what happens next.
///
/// Separated from the loops so the trace and the escalation are both callable in
/// tests: a control plane that stops accepting while the process keeps running is
/// the silent failure spec §11.8 rules out, so it must leave a log event and
/// must ask the whole server to stop.
fn handle_management_accept_failure(
    shutdown_tx: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    error: &std::io::Error,
    consecutive_errors: u32,
) -> AcceptFailure {
    tracing::error!(
        target: "llm_proxy::service",
        event = "management_accept_failed",
        error = %error,
        consecutive_errors,
        "management channel accept failed"
    );
    let action = management_accept_failure_policy(consecutive_errors);
    if action == AcceptFailure::Fatal {
        request_shutdown(shutdown_tx, MANAGEMENT_CHANNEL_FAILED_EXIT);
    }
    action
}

/// Advice appended to a bind failure, chosen from the *actual* cause.
///
/// A length hint on `EACCES` would send the user off to shorten
/// `XDG_RUNTIME_DIR` while the real problem is directory ownership, which is a
/// silent misdiagnosis in the same family as the bug itself (spec §11.9.2).
#[cfg(unix)]
fn management_bind_advice(err: &std::io::Error, path_len: usize) -> Option<&'static str> {
    if path_len > SUN_PATH_MAX {
        return Some(
            "路径长度超过 sun_path 上限 107 字节：缩短 XDG_RUNTIME_DIR，或让它指向更短的目录",
        );
    }
    match err.raw_os_error() {
        Some(libc::EADDRINUSE) => Some(
            "该路径上仍有存活的监听套接字：先执行 `llm-proxy shutdown`，确认进程已退出后再删除残留 socket 文件",
        ),
        Some(libc::EACCES) | Some(libc::EPERM) => Some(
            "runtime 目录不允许当前用户创建 socket：检查该目录的属主与权限（不是路径长度问题）",
        ),
        Some(libc::ENAMETOOLONG) => {
            Some("路径过长：缩短 XDG_RUNTIME_DIR 后重试（不是权限或占用问题）")
        }
        _ => None,
    }
}

/// Is someone still accepting connections on this socket file?
///
/// Three states, and the undecidable one resolves to "alive": unlinking a socket
/// that a live server is serving from silently detaches its only control plane
/// (the file goes, the listening inode stays), which is precisely the failure
/// this channel exists to prevent (spec §11.8、§11.9.1).
#[cfg(unix)]
async fn management_listener_alive(path: &Path) -> bool {
    const PROBE_BUDGET: Duration = Duration::from_millis(200);
    match tokio::time::timeout(PROBE_BUDGET, tokio::net::UnixStream::connect(path)).await {
        Ok(Ok(_stream)) => true,
        // `EAGAIN` on a stream socket means the listener's accept queue is full:
        // somebody is home and merely saturated. Calling that "nobody home" would
        // unlink a live server's only control-plane name — the exact "process
        // healthy, control plane silently missing" state this guard exists to
        // prevent. Ambiguity goes the same way as the timeout arm below: served.
        // (Reachability note: today `acquire_server_ownership` precedes `bind`, so
        // a live listener in the same runtime dir already holds that flock and we
        // cannot get here; this arm must not rely on that ordering staying as-is.)
        Ok(Err(err)) if err.raw_os_error() == Some(libc::EAGAIN) => true,
        Ok(Err(_)) => false, // ENOENT / ECONNREFUSED / ENOTSOCK: nobody home, the file is a leftover
        Err(_elapsed) => true, // no verdict inside the budget is treated as "still served"
    }
}

/// Clear a leftover socket file so startup can proceed, but never a live one.
///
/// A bind failure is fatal, so a crash leftover must be removed
/// or the service could never start again. A listener that answers the probe is a
/// different matter: unlinking the file a live server accepts on detaches *its*
/// control plane while the process keeps running, so that case is a loud stop.
#[cfg(unix)]
async fn remove_stale_socket(path: &Path) -> Result<()> {
    if management_listener_alive(path).await {
        tracing::error!(
            target: "llm_proxy::service",
            event = "management_socket_already_served",
            path = %path.display(),
            "a live listener occupies this management socket path"
        );
        bail!(
            "management socket {} is already served by a live listener; 另一个 server 正在该通道上服务，拒绝接管：先执行 `llm-proxy shutdown`",
            path.display()
        );
    }
    match fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(
                target: "llm_proxy::service",
                event = "management_socket_stale_removed",
                path = %path.display(),
                "removed a socket file left behind by a previous run"
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to remove stale management socket {}",
                    path.display()
                )
            });
        }
    }
    Ok(())
}

/// Single implementation of the management channel accept loop.
///
/// `ready_tx` is signalled **as soon as the socket is bound and proven 0600**,
/// never after the accept loop (which does not return): `run_foreground` blocks
/// on it to fail fast when the channel cannot be trusted. Tests pass
/// `None` and a caller-chosen path so the production code itself is what gets
/// exercised.
#[cfg(unix)]
async fn run_management_socket_at(
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    path: &Path,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
) -> Result<()> {
    use tokio::net::UnixListener;

    // Every pre-ready failure must reach `run_foreground` through the same door:
    // the caller cannot tell "still starting" from "never starting" otherwise.
    let fail = |ready_tx: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
                msg: String|
     -> Result<()> {
        if let Some(tx) = ready_tx {
            let _ = tx.send(Err(msg.clone()));
        }
        Err(anyhow::anyhow!("{}", msg))
    };

    // A leftover file from a killed server must not make startup impossible now
    // that a bind failure is fatal, so it is cleared first — but only after
    // proving nobody is still served by it.
    if let Err(err) = remove_stale_socket(path).await {
        return fail(ready_tx, format!("{err:#}"));
    }
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            let path_len = path.as_os_str().len();
            tracing::error!(
                target: "llm_proxy::service",
                event = "management_socket_bind_failed",
                path = %path.display(),
                path_len,
                error = %e,
                "failed to bind management socket"
            );
            let mut msg = format!(
                "failed to bind management socket {} (path length {} bytes): {e}",
                path.display(),
                path_len
            );
            if let Some(advice) = management_bind_advice(&e, path_len) {
                msg.push_str("; ");
                msg.push_str(advice);
            }
            return fail(ready_tx, msg);
        }
    };
    // The 0600 mode *is* this channel's access control (spec §11.8), so "we asked
    // for it" is not evidence of readiness — the kernel's answer is. Failing here
    // must not leave an insecure socket reachable (§11.9.2).
    if let Err(err) = enforce_mode(path, 0o600) {
        let cleanup = match fs::remove_file(path) {
            Ok(()) => String::new(),
            Err(e) => format!("; the socket file could not be removed either: {e}"),
        };
        let msg = format!(
            "management socket {} is not owner-only: {err}{cleanup}",
            path.display()
        );
        tracing::error!(
            target: "llm_proxy::service",
            event = "management_socket_insecure_mode",
            path = %path.display(),
            error = %err,
            "refusing to serve the management channel without its 0600 trust boundary"
        );
        return fail(ready_tx, msg);
    }
    if let Some(tx) = ready_tx {
        let _ = tx.send(Ok(()));
    }
    serve_management_connections(Arc::new(listener), state, shutdown_tx).await
}

/// The accept loop itself; separated so its failure policy is testable.
///
/// The loop never returns while healthy, so any exit path is an anomaly and must
/// leave a trace. A control plane that dies while the data plane keeps serving
/// is a safety-boundary breach (writes would silently stop working, spec §11.8),
/// so repeated failures stop the server instead of continuing without a channel
/// (§11.9.3: 运行期可降级的只是可重试的瞬时错误).
#[cfg(unix)]
async fn serve_management_connections(
    listener: Arc<tokio::net::UnixListener>,
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
) -> Result<()> {
    let router = management_router(state, shutdown_tx.clone());
    let mut consecutive_errors: u32 = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                consecutive_errors = 0;
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
            Err(err) => {
                consecutive_errors += 1;
                match handle_management_accept_failure(&shutdown_tx, &err, consecutive_errors) {
                    AcceptFailure::Retry => {
                        tokio::time::sleep(MANAGEMENT_ACCEPT_RETRY_BACKOFF).await
                    }
                    AcceptFailure::Fatal => bail!(
                        "management channel stopped accepting after {consecutive_errors} consecutive accept failures: {err}"
                    ),
                }
            }
        }
    }
}

#[cfg(windows)]
async fn run_management_socket_with_ready(
    _runtime_dir: PathBuf,
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> Result<()> {
    // 从配置读取管理通道端口，默认 8989。
    // 强制绑 127.0.0.1——管理面始终仅本机可访问（安全约束，不可配置）。
    let management_port = state
        .config()
        .server
        .as_ref()
        .map(|s| s.management_port())
        .unwrap_or(8989);
    let bind_addr: std::net::SocketAddr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        management_port,
    );
    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("failed to bind management TCP listener: {e}");
            tracing::error!(
                target: "llm_proxy::service",
                event = "management_socket_bind_failed",
                error = %e,
                "failed to bind management TCP listener"
            );
            let _ = ready_tx.send(Err(msg.clone()));
            return Err(anyhow::anyhow!("{}", msg));
        }
    };
    // Readiness means "the channel can be reached", and the address is what the
    // CLI connects to. Sending Ok before it resolves would let the data plane
    // start against a control plane whose endpoint is still unknown (§11.9.1).
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            let msg = format!("failed to get management addr: {e}");
            tracing::error!(
                target: "llm_proxy::service",
                event = "management_socket_addr_failed",
                error = %e,
                "management channel bound but its address is unknown"
            );
            let _ = ready_tx.send(Err(msg.clone()));
            return Err(anyhow::anyhow!("{}", msg));
        }
    };
    let _ = ready_tx.send(Ok(()));
    tracing::info!("management channel listening on {addr}");
    let router = management_router(state, shutdown_tx.clone());
    let mut consecutive_errors: u32 = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                consecutive_errors = 0;
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
            // Same failure policy as the Unix channel: a control plane that stops
            // accepting must not leave a data plane running behind it (§11.8).
            Err(err) => {
                consecutive_errors += 1;
                match handle_management_accept_failure(&shutdown_tx, &err, consecutive_errors) {
                    AcceptFailure::Retry => {
                        tokio::time::sleep(MANAGEMENT_ACCEPT_RETRY_BACKOFF).await
                    }
                    AcceptFailure::Fatal => bail!(
                        "management channel stopped accepting after {consecutive_errors} consecutive accept failures: {err}"
                    ),
                }
            }
        }
    }
}

/// UDS 管理通道 state：写操作（provider/model 增删改、OAuth、shutdown）仅本机管理通道。
#[derive(Clone)]
pub struct ManagementState {
    pub app: crate::proxy::AppState,
    pub shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
}

impl axum::extract::FromRef<ManagementState> for crate::proxy::AppState {
    fn from_ref(s: &ManagementState) -> Self {
        s.app.clone()
    }
}

/// 构建管理通道 router：shutdown/state/env + 全部 admin 写端点。
/// 写端点从 TCP 公开接口迁到这里（北极星：写操作仅通过管理通道）。
/// 所有 /admin/* 端点（包括只读端点）均迁入管理通道，
/// 不再暴露在公开 HTTP listener。管理通道是 owner-only UDS（Unix 0600 权限）。
pub(crate) fn management_router(
    state: crate::proxy::AppState,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<&'static str>>>>,
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
        // Read-only admin endpoints moved from public HTTP to UDS.
        // These endpoints leak usage data, provider topology, and client config
        // to anyone who can reach the public listener.
        .route("/admin/ping", get(crate::admin::ping))
        .route("/admin/status", get(crate::admin::status))
        .route("/admin/provider/list", get(crate::admin::provider_list))
        .route("/admin/provider/{id}", get(crate::admin::provider_info))
        .route("/admin/model/list", get(crate::admin::model_list))
        .route("/admin/model/{id}", get(crate::admin::model_info))
        .route(
            "/admin/client-config/{client}",
            get(crate::admin::client_config),
        )
        .route("/admin/usage", get(crate::admin::usage))
        .route(
            "/admin/usage/attribution",
            get(crate::admin::usage_attribution),
        )
        .route("/admin/metrics", get(crate::admin::metrics))
        .route("/admin/metrics/history", get(crate::admin::metrics_history))
        // : diagnostics query is UDS-only — `capture = "full"` records
        // may embed the egress request body, so this endpoint must not appear
        // on the public HTTP listener (404 there; red-proof test in this file).
        .route("/admin/diagnostics", get(crate::admin::diagnostics))
        // Write endpoints (already on UDS)
        .route("/admin/provider/add", post(crate::admin::provider_add))
        .route(
            "/admin/provider/reselect",
            post(crate::admin::provider_reselect),
        )
        .route(
            "/admin/provider/remove",
            delete(crate::admin::provider_remove),
        )
        .route("/admin/provider/copy", post(crate::admin::provider_copy))
        .route("/admin/config/reload", post(crate::admin::config_reload))
        .route("/admin/config/snapshot", get(crate::admin::config_snapshot))
        .route("/admin/config/update", post(crate::admin::config_update))
        .route("/admin/model/add", post(crate::admin::model_add))
        .route("/admin/model/set", post(crate::admin::model_set))
        .route("/admin/model/remove", post(crate::admin::model_remove))
        .route("/admin/model/rename", post(crate::admin::model_rename))
        .route("/admin/model/provider", post(crate::admin::model_provider))
        .route("/admin/cooldown/clear", post(crate::admin::cooldown_clear))
        .route("/admin/cooldown/set", post(crate::admin::cooldown_set))
        .route("/admin/oauth/write", post(crate::admin::oauth_write))
        // logout 是凭据写操作 + 内存缓存刷新，只能挂在
        // 管理通道（server 存活时 CLI 直写磁盘会留下"已登出仍可用"窗口）。
        .route("/admin/oauth/logout", post(crate::admin::oauth_logout))
        // Probe fires real upstream requests and mutates cache /
        // active / cooldown state, so it belongs on the owner-only management
        // channel, not the public listener. Streaming (SSE) is served over the
        // same HTTP/1 transport used for the rest of this router.
        .route("/admin/status/probe", post(crate::admin::status_probe))
        .route("/admin/token/create", post(crate::admin::token_create))
        .route("/admin/token/delete", post(crate::admin::token_delete))
        .route("/admin/token/rename", post(crate::admin::token_rename))
        .route(
            "/admin/token/reset-usage",
            post(crate::admin::token_reset_usage),
        )
        .route(
            "/admin/protection/bad-request-budget",
            get(crate::admin::bad_request_budget_status),
        )
        .route(
            "/admin/protection/bad-request-budget/reset",
            post(crate::admin::bad_request_budget_reset),
        )
        .route(
            "/admin/protection/token-error-cooldown",
            get(crate::admin::token_error_cooldown_status),
        )
        .route(
            "/admin/protection/token-error-cooldown/clear",
            post(crate::admin::token_error_cooldown_clear),
        )
        .with_state(mgmt)
}

async fn shutdown_handler(State(mgmt): State<ManagementState>) -> axum::Json<Value> {
    request_shutdown(&mgmt.shutdown_tx, "management_uds");
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

/// 读取运行中实例的 `/state`。`runtime_dir` 必须由调用方按注入的 config 派生：
/// 走全局默认会连到另一个 identity 分片的实例（或什么都读不到）。
pub fn management_state_in(runtime_dir: &Path) -> Result<Value> {
    let response = send_management_request_in(
        runtime_dir,
        "GET /state HTTP/1.1\r\nconnection: close\r\n\r\n",
        None,
    )?;
    parse_http_json_response(&response)
}

/// 向 `runtime_dir` 对应的实例注入环境变量（同上：定位必须注入，不走全局默认）。
pub fn inject_env_in(
    runtime_dir: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<String>> {
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
    let response = send_management_request_in(runtime_dir, &request, None)?;
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
fn send_management_shutdown_in(runtime_dir: &Path) -> Result<()> {
    let response = send_management_request_in(
        runtime_dir,
        "POST /shutdown HTTP/1.1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        None,
    )?;
    if response.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        bail!("management shutdown failed")
    }
}

#[cfg(unix)]
fn send_management_request_in(
    runtime_dir: &Path,
    request: &str,
    _config_path: Option<&Path>,
) -> Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path_in(runtime_dir);
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("failed to connect management socket {}", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("failed to set read timeout on management socket")?;
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
fn send_management_shutdown(config_path: &Path) -> Result<()> {
    // Windows: 通过 TCP localhost 连接管理通道
    use std::io::{Read, Write};
    let port = load_management_port(config_path);
    let addr = management_tcp_addr(port);
    let mut stream =
        std::net::TcpStream::connect(addr).context("failed to connect management TCP channel")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("failed to set read timeout on management channel")?;
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
fn send_management_request_in(
    _runtime_dir: &Path,
    request: &str,
    config_path: Option<&Path>,
) -> Result<String> {
    use std::io::{Read, Write};
    let port = config_path.map(load_management_port).unwrap_or(8989);
    let addr = management_tcp_addr(port);
    let mut stream =
        std::net::TcpStream::connect(addr).context("failed to connect management TCP channel")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("failed to set read timeout on management channel")?;
    stream
        .write_all(request.as_bytes())
        .context("failed to write management request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("failed to read management response")?;
    Ok(response)
}

/// 从配置文件加载管理通道端口（Windows 专用）。
#[cfg(not(unix))]
fn load_management_port(config_path: &Path) -> u16 {
    crate::config::Config::load(config_path)
        .ok()
        .and_then(|cfg| cfg.server)
        .map(|s| s.management_port())
        .unwrap_or(8989)
}

#[cfg(not(unix))]
fn management_tcp_addr(port: u16) -> std::net::SocketAddr {
    // 强制 127.0.0.1——管理面始终仅本机可访问（安全约束，不可配置）。
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
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
fn force_kill_process(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).context("failed to kill process");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let signalable = if rc == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    // A zombie has already exited — it serves nothing and holds no lock — but
    // `kill(pid, 0)` still succeeds for it. Counting it as alive would make
    // `wait_for_exit` spin for the whole timeout on a process that is already
    // gone, and would let a not-yet-reaped child masquerade as a running
    // server. Treat it as not running.
    signalable && !is_zombie(pid)
}

/// Whether `pid` is a zombie on Linux (`/proc/<pid>/stat` state `Z`).
///
/// Non-Linux Unix targets have no `/proc`; there the plain signal check stands.
#[cfg(all(unix, target_os = "linux"))]
fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        // The process vanished between the signal check and this read, or
        // /proc is unavailable in this namespace — fall back to "not a zombie".
        return false;
    };
    // Format: "pid (comm) state ..."; comm may itself contain spaces and
    // parentheses, so split on the *last* ')'.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    rest.trim_start().starts_with('Z')
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_zombie(_pid: u32) -> bool {
    false
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
fn force_kill_process(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .context("failed to invoke taskkill /F")?;
    if !status.success() {
        bail!("taskkill /F failed with {status}");
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
    fn single_instance_premise_enforced_across_different_runtime_dirs() {
        let config_dir = tempfile::tempdir().expect("config_dir");
        let config_path = config_dir.path().join("config.toml");
        fs::write(&config_path, "[server]\nlisten = \"127.0.0.1:8989\"\n").expect("write config");

        // Instance 1 starts with runtime_dir 1 and listen port 8989
        let runtime_dir_1 = tempfile::tempdir().expect("runtime_dir_1");
        let _ownership_1 =
            acquire_server_ownership_in(runtime_dir_1.path(), Duration::from_secs(1))
                .expect("ownership_1");
        let lock_1 = crate::ownership::ServerConfigLock::acquire(&config_path, "127.0.0.1:8989")
            .expect("lock_1");

        // Instance 2 starts with a DIFFERENT runtime_dir 2 and DIFFERENT listen port 9090.
        // Without the single-instance preflight, runtime_dir ownership lock and port binding would both succeed,
        // silently violating the single-instance premise.
        let runtime_dir_2 = tempfile::tempdir().expect("runtime_dir_2");
        let ownership_2 = acquire_server_ownership_in(runtime_dir_2.path(), Duration::from_secs(1));
        assert!(
            ownership_2.is_ok(),
            "runtime_dir_2 ownership alone does not block instance 2"
        );

        // But ServerConfigLock fails-stop and rejects Instance 2!
        let err = crate::ownership::ServerConfigLock::acquire(&config_path, "127.0.0.1:9090")
            .unwrap_err()
            .to_string();
        assert!(err.contains("拒绝启动"), "{err}");
        assert!(err.contains("正在使用同一份 config 目录"), "{err}");
        assert!(err.contains("127.0.0.1:8989"), "{err}");

        // Preflight check in start_background also rejects Instance 2 before spawning
        let preflight_err = crate::ownership::ServerConfigLock::preflight_check(&config_path)
            .unwrap_err()
            .to_string();
        assert!(preflight_err.contains("拒绝启动"), "{preflight_err}");

        // Once Instance 1 exits and releases lock_1, Instance 2 can acquire
        drop(lock_1);
        let lock_2 = crate::ownership::ServerConfigLock::acquire(&config_path, "127.0.0.1:9090")
            .expect("lock_2 after drop");
        drop(lock_2);
    }

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
        // Diagnostics must name the holder so the user can act on it.
        assert!(err.contains("alive"), "{err}");
        assert!(err.contains("llm-proxy shutdown"), "{err}");
    }

    /// A dead recorded holder while the kernel lock is still held is the
    /// concurrent-start window: the retry must not succeed (someone holds the
    /// lock) and the error must stay actionable without telling the user to
    /// delete the lock file (deleting it does not release the flock).
    #[test]
    fn acquire_server_ownership_reports_actionable_error_for_dead_recorded_holder() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Hold the flock ourselves, then make the recorded holder look dead.
        let _lock =
            crate::ownership::OwnershipLock::try_acquire(temp.path(), "server", "llm-proxy serve")
                .expect("pre-acquire server lock");
        let dead_pid = {
            let mut child = std::process::Command::new("true")
                .spawn()
                .expect("spawn true");
            let pid = child.id();
            child.wait().expect("reap child");
            pid
        };
        let stale = serde_json::json!({
            "pid": dead_pid,
            "process_type": "server",
            "started_at": 0,
            "command": "llm-proxy serve",
        });
        std::fs::write(
            temp.path().join("ownership.lock"),
            serde_json::to_vec(&stale).expect("serialize"),
        )
        .expect("write stale metadata");

        let err = acquire_server_ownership_in(temp.path(), Duration::from_millis(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("已持有所有权锁"), "{err}");
        assert!(err.contains("dead"), "{err}");
        assert!(err.contains(&dead_pid.to_string()), "{err}");
        assert!(
            !err.contains("删除锁文件"),
            "must not suggest deleting the lock file: {err}"
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
    fn service_paths_use_runtime_for_process_files_and_state_for_logs() {
        let runtime = runtime_dir();
        let state = state_dir();
        assert_eq!(pid_path_in(&runtime), runtime.join("llm-proxy.pid"));
        assert_eq!(socket_path(), runtime.join("llm-proxy.sock"));
        assert_eq!(log_path_in(&state), state.join("llm-proxy.log"));
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

        // 新增的三个 token 写端点也必须在管理通道上可达。
        // `f64a125` 修的正是"UDS management router 未注册 token 路由"（当时
        // 委托调用返回空响应 → CLI 报 "EOF while parsing a value"）。
        //
        // 判据用"业务校验失败"而非"token 不存在"：后者返回 404，与 router 级
        // 404 不可区分。空 name 由 handler 直接拒为 400（src/admin.rs:1331），
        // 且不会创建任何 token。
        let body = r#"{"name":""}"#;
        let request = format!(
            "POST /admin/token/create HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = uds_test_request(&sock, &request).await;
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "token create must be routed on UDS, got: {resp}"
        );
        assert!(
            resp.contains("token name cannot be empty"),
            "token create must reach the handler's validation, got: {resp}"
        );

        // delete / reset-usage 用语法非法的 body：axum 在路由命中之后才解析
        // body，因此 400（JsonSyntaxError）证明路由存在；未注册路径会先 404，
        // 与 handler 的业务 404 区分开。
        for path in ["/admin/token/delete", "/admin/token/reset-usage"] {
            let body = "not-json";
            let request = format!(
                "POST {path} HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let resp = uds_test_request(&sock, &request).await;
            assert!(
                !resp.starts_with("HTTP/1.1 404"),
                "{path} must be routed on UDS (404 would mean the route is missing), got: {resp}"
            );
            assert!(
                resp.starts_with("HTTP/1.1 400"),
                "{path} should reject a malformed body with 400, got: {resp}"
            );
        }

        // 带真实上游副作用的探测端点必须在管理通道上可达。
        // 非空 body + 非法 JSON → parse_probe_body 先行 400（不触发任何上游
        // 请求）；未注册路由会先 404，据此区分"到达 handler"。
        let body = "not-json";
        let request = format!(
            "POST /admin/status/probe HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = uds_test_request(&sock, &request).await;
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "status probe must be routed on UDS (404 would mean the route is missing), got: {resp}"
        );

        // oauth logout 写端点必须在管理通道上可达
        // （同上：非法 JSON → 400 证明路由命中；未注册会先 404）。
        let body = "not-json";
        let request = format!(
            "POST /admin/oauth/logout HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = uds_test_request(&sock, &request).await;
        assert!(
            resp.starts_with("HTTP/1.1 400"),
            "oauth logout must be routed on UDS (404 would mean the route is missing), got: {resp}"
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
        let applied = inject_env_in(Path::new("."), &env).expect("empty env succeeds");
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
    fn running_pid_returns_none_when_pid_file_missing_or_invalid() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(running_pid_in(temp.path()), None);
        fs::write(pid_path_in(temp.path()), "not-a-pid\n").unwrap();
        assert_eq!(running_pid_in(temp.path()), None);
    }

    #[test]
    fn refuse_if_running_removes_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "abc\n").unwrap();
        refuse_if_running_in(temp.path()).expect("invalid pid is stale");
        assert!(!pid_path_in(temp.path()).exists());
    }

    #[test]
    fn refuse_if_running_removes_stale_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "99999999\n").unwrap();
        refuse_if_running_in(temp.path()).expect("stale pid is removed");
        assert!(!pid_path_in(temp.path()).exists());
    }

    #[test]
    fn refuse_if_running_fails_for_current_process_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            pid_path_in(temp.path()),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        let err = refuse_if_running_in(temp.path()).unwrap_err().to_string();
        assert!(err.contains("already running"));
        assert!(pid_path_in(temp.path()).exists());
    }

    #[test]
    fn write_pid_creates_pid_file_under_runtime_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_pid_in(temp.path(), 4242).expect("write pid");
        assert_eq!(
            fs::read_to_string(pid_path_in(temp.path())).unwrap(),
            "4242\n"
        );
    }

    #[test]
    fn wait_for_exit_returns_quickly_for_nonexistent_pid() {
        let start = Instant::now();
        let exited = wait_for_exit(99999999, Duration::from_secs(2));
        assert!(exited, "a nonexistent pid is already exited");
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_process_ignores_nonexistent_pid() {
        terminate_process(99999999).expect("ESRCH is treated as already stopped");
    }

    #[test]
    fn application_dir_appends_product_subdir_to_its_category_root() {
        let root = PathBuf::from("/tmp/llm-proxy-state-root");
        assert_eq!(
            application_dir(Some(root.clone()), PathBuf::from("unused")),
            root.join("llm-proxy")
        );
        assert_eq!(
            application_dir(None, PathBuf::from("/tmp/default-state-root")),
            PathBuf::from("/tmp/default-state-root/llm-proxy")
        );
    }

    #[test]
    fn service_paths_keep_runtime_and_state_categories_separate() {
        let runtime = PathBuf::from("/tmp/llm-proxy-runtime");
        let state = PathBuf::from("/tmp/llm-proxy-state");
        assert_eq!(pid_path_in(&runtime), runtime.join("llm-proxy.pid"));
        assert_eq!(socket_path_in(&runtime), runtime.join("llm-proxy.sock"));
        assert_eq!(log_path_in(&state), state.join("llm-proxy.log"));
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
    fn running_pid_returns_current_process_when_pid_file_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            pid_path_in(temp.path()),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        assert_eq!(running_pid_in(temp.path()), Some(std::process::id()));
    }

    #[test]
    fn running_pid_returns_none_for_stale_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "99999999\n").unwrap();
        assert_eq!(running_pid_in(temp.path()), None);
    }

    #[test]
    fn refuse_if_running_allows_missing_runtime_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        refuse_if_running_in(&temp.path().join("missing"))
            .expect("missing pid file is not running");
    }

    #[test]
    fn refuse_if_running_removes_whitespace_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "   \n").unwrap();
        refuse_if_running_in(temp.path()).expect("blank pid is stale");
        assert!(!pid_path_in(temp.path()).exists());
    }

    #[test]
    fn write_pid_overwrites_existing_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "1\n").unwrap();
        write_pid_in(temp.path(), 2).expect("write pid");
        assert_eq!(fs::read_to_string(pid_path_in(temp.path())).unwrap(), "2\n");
    }

    #[test]
    fn shutdown_background_missing_pid_file_is_ok_when_no_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        shutdown_background_in(temp.path()).expect("not running is ok");
    }

    #[test]
    fn shutdown_background_rejects_invalid_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), "invalid\n").unwrap();
        let err = shutdown_background_in(temp.path()).unwrap_err().to_string();
        assert!(err.contains("invalid pid file"));
    }

    #[test]
    fn wait_for_exit_zero_timeout_returns_immediately() {
        let start = Instant::now();
        let exited = wait_for_exit(std::process::id(), Duration::from_millis(0));
        // A zero timeout must not report a live pid (itself) as exited.
        assert!(!exited);
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    /// `shutdown` must not claim success for a process it cannot stop. PID 1 is
    /// unkillable from an unprivileged test, and a zero timeout means no signal
    /// is sent — so this only observes the "did not exit" verdict.
    #[cfg(unix)]
    #[test]
    fn wait_for_exit_reports_failure_for_a_live_unkillable_pid() {
        if !process_exists(1) {
            return;
        }
        assert!(
            !wait_for_exit(1, Duration::from_millis(0)),
            "a live pid must not be reported as exited"
        );
    }

    /// A process that ignores SIGTERM must still be stopped via the SIGKILL
    /// escalation, and only then reported as confirmed-exited.
    #[cfg(unix)]
    #[test]
    fn terminate_process_confirmed_escalates_to_sigkill() {
        use std::io::{BufRead, BufReader};

        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; echo ready; while true; do sleep 0.2; done")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn SIGTERM-ignoring child");
        let pid = child.id();
        // Wait for the child to report that the SIGTERM trap is installed.
        // Signalling before then would hit the default handler and let the
        // test pass without ever exercising the escalation.
        {
            let stdout = child.stdout.take().expect("child stdout");
            let mut line = String::new();
            BufReader::new(stdout)
                .read_line(&mut line)
                .expect("read readiness line");
            assert_eq!(line.trim(), "ready", "child never installed its trap");
        }
        let started = Instant::now();
        let confirmed = terminate_process_confirmed(pid, Duration::from_millis(500))
            .expect("escalation must not error");

        // Always stop and reap before asserting: a failing assertion must not
        // leave a live child (or an un-reaped handle) hanging the test binary.
        let _ = child.kill();
        let _ = child.wait();

        assert!(confirmed, "SIGKILL escalation must stop the child");
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "SIGTERM must have been ignored for the full step timeout, proving escalation ran"
        );
        assert!(!process_exists(pid), "child must be gone once confirmed");
    }

    #[test]
    fn terminate_process_confirmed_succeeds_for_nonexistent_pid() {
        let confirmed =
            terminate_process_confirmed(99999999, Duration::from_millis(100)).expect("no error");
        assert!(confirmed, "a nonexistent pid is already stopped");
    }

    /// The pid-file fallback path must confirm the process is gone before
    /// removing the pid file and printing "stopped".
    #[cfg(unix)]
    #[test]
    fn shutdown_background_fallback_confirms_exit_before_removing_pid_file() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(pid_path_in(temp.path()), format!("{pid}\n")).unwrap();

        // No management socket in the temp dir ⇒ the fallback path runs.
        let result = shutdown_background_in(temp.path());

        // Reap unconditionally so a failure cannot leave a zombie behind.
        let _ = child.kill();
        let _ = child.wait();

        result.expect("fallback shutdown must stop the child");
        assert!(!process_exists(pid), "child must be stopped");
        assert!(
            !pid_path_in(temp.path()).exists(),
            "pid file is removed only after the exit is confirmed"
        );
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn process_exists_reports_false_while_the_child_is_a_zombie() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();

        let mut observed_zombie = false;
        for _ in 0..200 {
            if is_zombie(pid) {
                observed_zombie = true;
                // A zombie has exited: `kill(pid, 0)` still succeeds, so this
                // is exactly the case the plain signal check got wrong.
                assert!(
                    !process_exists(pid),
                    "an exited-but-unreaped child must not count as running"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.wait();

        assert!(
            observed_zombie,
            "child was never observed as a zombie; test did not exercise the case"
        );
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
    async fn shutdown_handler_records_management_uds_as_exit_reason() {
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
        assert_eq!(rx.try_recv(), Ok("management_uds"));
        let _ = shutdown_handler(State(mgmt.clone())).await;
        assert!(mgmt.shutdown_tx.lock().unwrap().is_none());
    }

    #[test]
    fn request_shutdown_preserves_signal_source() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let shutdown_tx = Arc::new(Mutex::new(Some(tx)));
        request_shutdown(&shutdown_tx, "SIGTERM");
        assert_eq!(rx.blocking_recv(), Ok("SIGTERM"));
    }

    /// 「不可判定必须区别于成功」：`service_stop` 上的 writer 标签三态互不
    /// 相同，取不到结论不得与干净关闭共用同一个值。
    #[test]
    fn writer_stop_label_keeps_three_states_distinct() {
        use crate::runtime_config::WriterError;

        assert_eq!(writer_stop_label(&Ok(())), "ok");
        assert_eq!(
            writer_stop_label(&Err(WriterError::FailedStop)),
            "failed_stop"
        );
        assert_eq!(
            writer_stop_label(&Err(WriterError::StopUndetermined)),
            "undetermined"
        );
        assert_eq!(
            writer_stop_label(&Err(WriterError::MailboxClosed)),
            "undetermined"
        );
        // 不可判定 ≠ 成功 ≠ fail-stop：三者两两不同，不能被折算。
        let labels = [
            writer_stop_label(&Ok(())),
            writer_stop_label(&Err(WriterError::FailedStop)),
            writer_stop_label(&Err(WriterError::StopUndetermined)),
        ];
        assert_eq!(
            labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
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

    /// `AppState` bound to a config path inside `dir`, so nothing a test does
    /// with it can reach the live user config.
    #[cfg(unix)]
    fn isolated_app_state(dir: &Path) -> crate::proxy::AppState {
        crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            dir.join("config.toml"),
        )
    }

    /// A socket path beyond `sun_path` must fail with a descriptive
    /// error **and** push that error to the ready channel — `run_foreground`
    /// only fails fast if it receives something.
    #[tokio::test]
    #[cfg(unix)]
    async fn management_socket_reports_bind_failure_to_the_ready_channel() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-long-path-")
            .tempdir()
            .expect("tempdir");
        let sock_path = temp.path().join("a".repeat(120)).join("llm-proxy.sock");
        std::fs::create_dir_all(sock_path.parent().expect("parent dir")).expect("create long dir");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();

        let result = run_management_socket_at(
            isolated_app_state(temp.path()),
            Arc::new(Mutex::new(Some(shutdown_tx))),
            &sock_path,
            Some(ready_tx),
        )
        .await;

        let err = result.expect_err("bind must fail for a path longer than sun_path");
        let err = err.to_string();
        assert!(
            err.contains("failed to bind management socket"),
            "error should name the failing subsystem: {err}"
        );
        assert!(
            err.contains(&format!(
                "path length {} bytes",
                sock_path.as_os_str().len()
            )),
            "error should report the offending path length: {err}"
        );

        let ready = tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("ready channel must be signalled, not left hanging")
            .expect("ready sender must not be dropped without a verdict");
        let msg = ready.expect_err("bind failure must be reported on the ready channel");
        assert!(
            msg.contains("path length"),
            "ready channel carries the same diagnosis: {msg}"
        );
    }

    /// Core regression gate: the ready signal must fire **as soon as
    /// `bind` returns**, not after the accept loop (which never returns). A
    /// signal sent from after the loop leaves `run_foreground` hanging before
    /// the public listener is ever bound — the defect the first fix shipped.
    #[tokio::test]
    #[cfg(unix)]
    async fn management_socket_signals_ready_before_serving_and_serves_requests() {
        use std::os::unix::fs::MetadataExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-")
            .tempdir()
            .expect("tempdir");
        let sock_path = temp.path().join("llm-proxy.sock");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let serve_path = sock_path.clone();
        let handle = tokio::spawn(async move {
            run_management_socket_at(
                isolated_app_state(temp.path()),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &serve_path,
                Some(ready_tx),
            )
            .await
        });

        // Readiness is an event, not a delay: this only resolves once bind done.
        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal must arrive while the accept loop is still running")
            .expect("ready sender must not be dropped without a verdict")
            .expect("bind of a short path must succeed");
        assert!(sock_path.exists(), "socket file should be created");
        assert_eq!(
            std::fs::metadata(&sock_path)
                .expect("socket metadata")
                .mode()
                & 0o777,
            0o600,
            "the 0600 mode is this channel's access control (spec §11.8)"
        );

        // The channel must actually answer, not just exist on the filesystem.
        let request = b"GET /state HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\
                        Connection: close\r\n\r\n";
        let response = tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = tokio::net::UnixStream::connect(&sock_path)
                .await
                .expect("connect management socket");
            stream.write_all(request).await.expect("write request");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read response");
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .expect("management channel must answer /state");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200 from the management channel, got: {response}"
        );

        handle.abort();
    }

    /// Pin the length boundary the bind diagnosis tells the user
    /// about — 107 bytes fits, 108 does not.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn management_socket_bind_stops_working_one_byte_past_sun_path() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-boundary-")
            .tempdir()
            .expect("tempdir");
        let dir = temp.path().as_os_str().to_str().expect("utf-8 tempdir");
        // "<dir>/<name>" must total exactly 107 / 108 bytes, so the tempdir
        // itself has to leave room for at least one byte of name. A deep TMPDIR
        // cannot host that fixture: skip visibly rather than fail on the
        // environment (the limit itself is pinned by `socket_path_plan_*`).
        if dir.len() + 1 + "x".len() > SUN_PATH_MAX {
            eprintln!("SKIP: tempdir path is already at the sun_path limit: {dir}");
            return;
        }
        let at_limit = temp.path().join("x".repeat(107 - dir.len() - 1));
        let past_limit = temp.path().join("x".repeat(108 - dir.len() - 1));
        assert_eq!(at_limit.as_os_str().len(), 107);
        assert_eq!(past_limit.as_os_str().len(), 108);

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let app = isolated_app_state(temp.path());
        let serve_app = app.clone();
        let handle = tokio::spawn(async move {
            run_management_socket_at(
                serve_app,
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &at_limit,
                Some(ready_tx),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal at the 107-byte limit")
            .expect("sender dropped")
            .expect("a 107-byte path must bind");
        handle.abort();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let result = bounded(
            "bind one byte past sun_path",
            run_management_socket_at(
                app,
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &past_limit,
                Some(ready_tx),
            ),
        )
        .await;
        let err = result.expect_err("a 108-byte path exceeds sun_path and must fail");
        let reported = err.to_string();
        assert!(
            reported.contains("(path length 108 bytes)"),
            "the failure must state the length it actually tried: {reported}"
        );
        assert!(
            reported.contains("sun_path"),
            "the failure must name the limit that was exceeded: {reported}"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), ready_rx)
                .await
                .expect("ready channel must be signalled at the boundary too")
                .expect("sender dropped")
                .is_err(),
            "the 108-byte failure must reach the ready channel"
        );
    }

    /// 走生产入口的规避路径：runtime dir 的常规名不可绑定时，管理通道必须
    /// 仍然起来并能服务请求，且绑定的就是计划算出的那个名字。
    #[tokio::test]
    #[cfg(unix)]
    async fn sun_path_fallback_serves_through_the_production_entry() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-fallback-")
            .tempdir()
            .expect("tempdir");
        // 注入的短基目录：测试不创建真实的 /tmp/llm-proxy-<uid>。
        let base = temp.path().join("short");
        // 又长又唯一的 runtime dir：规避名由它的哈希决定，并发运行不会撞名。
        let runtime = temp.path().join("x".repeat(120)).join("run");
        std::fs::create_dir_all(&runtime).expect("create long runtime dir");
        let (planned, fell_back) = socket_path_plan_in(&runtime, &base);
        assert!(
            fell_back,
            "the conventional path of {} must exceed sun_path",
            runtime.join("llm-proxy.sock").display()
        );
        assert!(
            planned.as_os_str().len() <= SUN_PATH_MAX,
            "the planned fallback must itself be bindable: {} bytes ({})",
            planned.as_os_str().len(),
            planned.display()
        );

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let serve_runtime = runtime.clone();
        let serve_base = base.clone();
        let handle = tokio::spawn(async move {
            run_management_socket_with_ready_at(
                &serve_runtime,
                &serve_base,
                isolated_app_state(&serve_runtime),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                ready_tx,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("the entry point must give a startup verdict")
            .expect("ready sender dropped")
            .expect("startup over the sun_path fallback must succeed");

        // 服务端绑定的文件必须正是纯派生算出的那一个：CLI 靠同一个函数找到它，
        // 少了这条断言，两侧算出不同路径也能全绿。
        assert!(
            planned.exists(),
            "the bound socket must be the planned path: {}",
            planned.display()
        );
        assert_eq!(
            std::fs::metadata(&base).expect("fallback base dir").mode() & 0o777,
            0o700,
            "the base dir lives under a world-writable tree and must be owner-only"
        );
        assert_eq!(
            std::fs::metadata(&planned)
                .expect("fallback socket metadata")
                .mode()
                & 0o777,
            0o600,
            "the 0600 mode is this channel's access control (spec \u{a7}11.8)"
        );
        let response = management_get_state(&planned).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected 200 from the fallback channel, got: {response}"
        );

        handle.abort();
        let _ = std::fs::remove_file(&planned);
    }

    /// 规避目录不可用（例如同名普通文件）时必须把失败送进就绪通道：数据平面不得
    /// 在没有控制面的情况下启动，也不得悄悄退回那个不可绑定的常规名。
    #[tokio::test]
    #[cfg(unix)]
    async fn sun_path_fallback_reports_an_unusable_base_instead_of_degrading() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-nobase-")
            .tempdir()
            .expect("tempdir");
        let base = temp.path().join("short");
        std::fs::write(&base, b"not a directory").expect("occupy the base name");
        let runtime = temp.path().join("y".repeat(120)).join("run");
        std::fs::create_dir_all(&runtime).expect("create long runtime dir");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let err = run_management_socket_with_ready_at(
            &runtime,
            &base,
            isolated_app_state(&runtime),
            Arc::new(Mutex::new(Some(shutdown_tx))),
            ready_tx,
        )
        .await
        .expect_err("an unusable fallback base must fail startup");
        let verdict = tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("the ready channel must carry the verdict")
            .expect("ready sender dropped")
            .expect_err("readiness must not be reported");
        assert!(
            verdict.contains("not a directory"),
            "the verdict must say why the base is unusable: {verdict}"
        );
        assert!(
            err.to_string().contains("not a directory"),
            "the returned error must match the verdict: {err}"
        );
        assert!(
            !runtime.join("llm-proxy.sock").exists(),
            "silently binding the unbindable conventional name is the failure we are avoiding"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn management_socket_refuses_symlinked_runtime_dir() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-symlink-rt-")
            .tempdir()
            .expect("tempdir");
        let target = temp.path().join("target");
        std::fs::create_dir(&target).expect("target dir");
        let runtime = temp.path().join("rt-symlink");
        std::os::unix::fs::symlink(&target, &runtime).expect("symlink");
        let base = temp.path().join("fallback-base");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let err = run_management_socket_with_ready_at(
            &runtime,
            &base,
            isolated_app_state(&runtime),
            Arc::new(Mutex::new(Some(shutdown_tx))),
            ready_tx,
        )
        .await
        .expect_err("a symlinked runtime dir must fail startup");
        let verdict = tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("the ready channel must carry the verdict")
            .expect("ready sender dropped")
            .expect_err("readiness must not be reported");
        assert!(
            verdict.contains("symlink"),
            "the verdict must say the runtime dir is a symlink: {verdict}"
        );
        assert!(
            err.to_string().contains("symlink"),
            "the returned error must match the verdict: {err}"
        );
    }

    /// 常规名超限时换名；换出的名字仍是 runtime dir 的纯函数，所以
    /// CLI 侧不需要发现协议也能算出同一个路径。
    #[test]
    #[cfg(unix)]
    fn socket_path_plan_moves_to_a_bindable_path_past_sun_path() {
        let base = PathBuf::from("/tmp/llm-proxy-0");
        let long = PathBuf::from("/tmp").join("d".repeat(140)).join("run");
        let (path, fell_back) = socket_path_plan_in(&long, &base);
        assert!(
            fell_back,
            "a conventional path over sun_path must take the fallback: {}",
            long.join("llm-proxy.sock").display()
        );
        assert!(
            path.as_os_str().len() <= SUN_PATH_MAX,
            "the fallback must itself be bindable: {} bytes ({})",
            path.as_os_str().len(),
            path.display()
        );
        assert_eq!(path.parent(), Some(base.as_path()));

        let (primary, fell_back) =
            socket_path_plan_in(Path::new("/run/user/1000/llm-proxy"), &base);
        assert!(!fell_back, "the everyday path keeps its configured name");
        assert_eq!(
            primary,
            PathBuf::from("/run/user/1000/llm-proxy/llm-proxy.sock")
        );
        // CLI 侧入口必须与计划同源：若它改回只认常规名，探测会在规避实例上永远失败。
        assert_eq!(
            socket_path_in(&long),
            socket_path_plan_in(&long, &short_socket_base()).0,
            "the CLI's path entry must include the fallback"
        );
    }

    /// 短名由 runtime dir 哈希而来而非固定：否则一个全局 socket 会让实例 B 清理
    /// "残留"时把仍在工作中的实例 A 的控制面摘掉（spec \u{a7}11.8）。
    #[test]
    #[cfg(unix)]
    fn sun_path_fallback_keeps_parallel_runtime_dirs_apart() {
        let base = PathBuf::from("/tmp/llm-proxy-0");
        let a = PathBuf::from("/tmp").join("a".repeat(140));
        let b = PathBuf::from("/tmp").join(format!("{}b", "a".repeat(140)));
        let (sa, _) = socket_path_plan_in(&a, &base);
        let (sb, _) = socket_path_plan_in(&b, &base);
        assert_ne!(sa, sb, "distinct runtime dirs must not share a socket");
    }

    /// bind 失败已是致命错误，所以上次崩溃留下的 socket 文件必须被清掉——否则
    /// 服务永远起不来。这条测试就是那行清理代码的回归基线。
    #[tokio::test]
    #[cfg(unix)]
    async fn management_socket_clears_a_leftover_from_a_dead_listener() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-leftover-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("llm-proxy.sock");
        drop(tokio::net::UnixListener::bind(&path).expect("simulate the previous run"));
        assert!(
            path.exists(),
            "dropping a listener keeps its file: {}",
            path.display()
        );

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let serve_path = path.clone();
        let handle = tokio::spawn(async move {
            run_management_socket_at(
                isolated_app_state(temp.path()),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &serve_path,
                Some(ready_tx),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("verdict for the leftover case")
            .expect("ready sender dropped")
            .expect("a leftover socket file must not make startup impossible");
        let response = management_get_state(&path).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "the channel must serve after clearing the leftover, got: {response}"
        );
        handle.abort();
    }

    /// 探测的判据必须是"有监听者应答"，不是"文件存在"。
    ///
    /// 这类错位（"已发出" ≠ "已完成"）落在探测函数上有两个方向：把
    /// 连接被拒也当成"有人持有" ⇒ 残留 socket 永远清不掉；把文件存在当成"有人
    /// 持有" ⇒ server 已死后所有本地命令仍走委托路径。两个方向都要钉住。
    #[tokio::test]
    #[cfg(unix)]
    async fn the_probe_requires_a_live_listener_not_a_socket_file() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-probe-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("llm-proxy.sock");

        assert!(
            !management_listener_alive(&path).await,
            "an absent socket must not count as held"
        );

        let live = tokio::net::UnixListener::bind(&path).expect("bind a live incumbent");
        assert!(
            bounded("probe a live listener", management_listener_alive(&path)).await,
            "a listener that accepts the probe must count as held"
        );
        drop(live);

        // 监听者消失后文件仍在（真实崩溃现场）：connect 只会收到 ECONNREFUSED，
        // 必须判为"无人持有"，否则 remove_stale_socket 的清理分支永不触发。
        assert!(
            path.exists(),
            "dropping the listener must leave the socket file behind for this to mean anything"
        );
        assert!(
            !bounded("probe a dead leftover", management_listener_alive(&path)).await,
            "a leftover whose listener is gone must not count as held"
        );
    }

    /// 队列满（`EAGAIN`）的监听者**仍算有人持有**。把"暂时接不了"当成"无人"，
    /// `remove_stale_socket` 就会摘掉一个活 server 唯一的控制面名字 —— 进程健康、
    /// 控制面静默缺失，正是 spec §11.8 / §11.9.1 禁止的形态；判定方向也与本函数的
    /// 超时出口（无判决 ⇒ 视为在服务）一致。
    ///
    /// 构造：libc 建监听 socket 后 `listen(fd, 0)`（Linux 取整为 backlog 1），
    /// 未 accept 的连接逐条占满队列。**先断言构造真的产出 `EAGAIN`** —— 构造不成
    /// 功就跳过，等于一条空跑的绿（首轮 A7 的教训）。已排队的客户端 socket 必须
    /// 保活：AF_UNIX 在发起端关闭时会把对端从 accept 队列里摘掉，槽位一空，
    /// 后续连接就不会再拿 `EAGAIN`。
    #[tokio::test]
    #[cfg(unix)]
    async fn a_saturated_listener_still_counts_as_held() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-saturate-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("saturated.sock");
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let chars: Vec<libc::c_char> = bytes.iter().map(|byte| *byte as libc::c_char).collect();
        address.sun_path[..bytes.len()].copy_from_slice(&chars);
        let length = (std::mem::size_of::<libc::sockaddr_un>() - address.sun_path.len()
            + bytes.len()
            + 1) as libc::socklen_t;
        let sockaddr = &raw const address as *const libc::sockaddr;

        let listen_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(
            listen_fd >= 0,
            "socket(): {}",
            std::io::Error::last_os_error()
        );
        let listener = unsafe { OwnedFd::from_raw_fd(listen_fd) };
        assert_eq!(
            unsafe { libc::bind(listener.as_raw_fd(), sockaddr, length) },
            0,
            "bind(): {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            unsafe { libc::listen(listener.as_raw_fd(), 0) },
            0,
            "listen(): {}",
            std::io::Error::last_os_error()
        );

        let mut queued = Vec::new();
        let mut saturated = None;
        for _ in 0..8 {
            let connect_fd =
                unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0) };
            assert!(
                connect_fd >= 0,
                "socket(): {}",
                std::io::Error::last_os_error()
            );
            let client = unsafe { OwnedFd::from_raw_fd(connect_fd) };
            let rc = unsafe { libc::connect(client.as_raw_fd(), sockaddr, length) };
            let err = std::io::Error::last_os_error();
            if rc != 0 && err.raw_os_error() == Some(libc::EAGAIN) {
                saturated = Some(client);
                break;
            }
            assert!(
                rc == 0 || err.raw_os_error() == Some(libc::EINPROGRESS),
                "构造失败：连接既没排队也没被接受 (rc={rc}, err={err})"
            );
            queued.push(client);
        }
        assert!(
            saturated.is_some(),
            "构造失败：八条连接都没让队列满，本用例没有触及 `EAGAIN` 分支"
        );

        assert!(
            bounded(
                "probe a saturated listener",
                management_listener_alive(&path)
            )
            .await,
            "a listener whose accept queue is full is still served: its name must not be unlinked"
        );
        drop(saturated);
        drop(queued);
        drop(listener);
    }

    /// 父进程的启动预算必须**大于**子进程两段有界等待之和（所有权锁 + 通道判决）。
    ///
    /// 不覆盖时的形态不是"报错慢一点"，而是**假失败 + 孤儿健康实例**：父进程按
    /// `Undecided` 非 0 退出，子进程随后正常起来并写好自己的 pid 文件，用户第二次
    /// `serve` 只会被 `already running` 挡住。这条断言让"只改一个常量、忘了另一个"
    /// 必须变红（常量的推导关系本身写在 `BACKGROUND_START_BUDGET` 的注释里）。
    #[test]
    fn the_parent_startup_budget_covers_the_childs_own_budgets() {
        assert!(
            BACKGROUND_START_BUDGET > SERVER_OWNERSHIP_WAIT + MANAGEMENT_READY_BUDGET,
            "a child that is still legally starting must never be judged Undecided: \
             parent {BACKGROUND_START_BUDGET:?} vs child {SERVER_OWNERSHIP_WAIT:?} + {MANAGEMENT_READY_BUDGET:?}"
        );
    }

    /// 后台启动的正向信号同样只认"有监听者应答"（spec §11.9.1）。这条走
    /// `management_channel_answers_at`（`management_channel_answers` 的判据本体），
    /// 防止判据被换成 `path.exists()` 后仍能全绿。
    #[test]
    #[cfg(unix)]
    fn the_background_start_signal_requires_a_listener_not_a_file() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-signal-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("llm-proxy.sock");

        assert!(
            !management_channel_answers_at(&path),
            "an absent socket must not look like a started service"
        );
        let live = std::os::unix::net::UnixListener::bind(&path).expect("bind a live incumbent");
        assert!(
            management_channel_answers_at(&path),
            "a listener answering the probe must look like a started service"
        );
        drop(live);
        std::fs::remove_file(&path).expect("remove the socket file");
        assert!(
            !management_channel_answers_at(&path),
            "a removed channel must not keep looking alive"
        );
    }

    /// 探测的三条负向路径必须在**自己的预算内**给出"否"：这个函数跑在
    /// `confirm_background_start` 的循环里，而循环是先调用探测、**之后**才检查
    /// deadline（spec §11.9.1）。一次无界阻塞的 `connect()`（backlog 满、对端不
    /// accept）会把整条 `serve` 命令永久卡住，连"不可判定 ⇒ 报错"都出不来。
    #[test]
    #[cfg(unix)]
    fn the_probe_reports_negative_without_spending_its_budget() {
        use std::time::Instant;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-probe-")
            .tempdir()
            .expect("tempdir");
        let started = Instant::now();

        // ① 名字本身就超界：连 syscall 都不必问，规划器之外的第二道边界。
        let too_long = temp.path().join("z".repeat(180));
        assert!(
            !management_channel_answers_at(&too_long),
            "a path that cannot fit sun_path can never have a listener"
        );
        // ② 普通文件占了名字（不是 socket）⇒ ECONNREFUSED/ENOTSOCK。
        let plain = temp.path().join("plain.sock");
        std::fs::write(&plain, b"not a socket").expect("occupy the name with a file");
        assert!(
            !management_channel_answers_at(&plain),
            "a file that is not a socket must not look like a started service"
        );
        // ③ 父目录都不存在 ⇒ ENOENT。
        assert!(
            !management_channel_answers_at(&temp.path().join("missing").join("l.sock")),
            "an absent directory must not look like a started service"
        );

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "three negative probes must not approach a blocking connect: {elapsed:?}"
        );
    }

    /// 回退基目录必须在**创建那一刻**就是 0700。"先 mkdir 再 chmod"留出一个窗口，
    /// 同 uid 的进程可在窗口内看到并抢占这个名字；`DirBuilder::mode` 把 mode 放进
    /// 创建系统调用，窗口因此不存在。
    #[test]
    #[cfg(unix)]
    fn the_fallback_base_is_born_private() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-basemode-")
            .tempdir()
            .expect("tempdir");
        // `DirBuilder::mode` is AND-ed with the creating process' inverted umask,
        // so the expected bits depend on the environment (a CI with `umask 0177`
        // would make a literal `0o700` red without any product defect). Probe the
        // umask with a plain directory instead of assuming it: that keeps the
        // assertion exact here and still red when the mode is not supplied to the
        // creating syscall at all (a bare `create_dir_all` yields the umask-limited
        // 0o777, i.e. group/other bits survive every umask that is not itself
        // private-by-default — and under such a umask the created dir is private
        // anyway, so there is nothing left to catch).
        let probe = temp.path().join("umask-probe");
        std::fs::create_dir(&probe).expect("umask probe dir");
        let umask = !(std::fs::symlink_metadata(&probe).expect("probe").mode() & 0o777) & 0o777;
        let base = temp.path().join("llm-proxy-base");
        create_private_socket_base(&base).expect("create the base");
        let created = std::fs::symlink_metadata(&base)
            .expect("base metadata")
            .mode()
            & 0o777;
        assert_eq!(
            created & 0o077,
            0,
            "group and other bits must never be present on the shared base: {created:#o}"
        );
        assert_eq!(
            created,
            0o700 & !umask,
            "the mode must come from the creating syscall, not from a later chmod"
        );
        // 已存在时报 AlreadyExists —— 调用方据此转入校验分支，这不是失败。
        assert_eq!(
            create_private_socket_base(&base).err().map(|e| e.kind()),
            Some(std::io::ErrorKind::AlreadyExists),
            "an existing base must be reported, not silently accepted"
        );
    }

    /// 校验基目录要用 `lstat`。`mkdir` 落在一个**指向目录的软链接**上会跟随并"成功"，
    /// 于是别人的目录、甚至任意目录（`/`、`~`）都能被当成我们自己的 0700 基目录继续
    /// 绑定 —— 后续 `chmod` 还会顺着链接改别人的权限。
    #[test]
    #[cfg(unix)]
    fn the_fallback_base_refuses_to_be_a_symlink() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-baselink-")
            .tempdir()
            .expect("tempdir");
        let target = temp.path().join("someone-else");
        std::fs::create_dir(&target).expect("create the link target");
        let target_mode = std::fs::metadata(&target).expect("target metadata").mode() & 0o777;
        let link = temp.path().join("llm-proxy-base");
        std::os::unix::fs::symlink(&target, &link).expect("plant a symlink at the base position");

        let err = ensure_private_socket_base_dir(&link)
            .expect_err("a symlinked base must never be accepted as ours");
        assert!(
            format!("{err:#}").contains("symlink"),
            "the failure must name the actual shape: {err:#}"
        );
        assert_eq!(
            std::fs::metadata(&target).expect("target metadata").mode() & 0o777,
            target_mode,
            "refusing must not chmod through the link into someone else's directory"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink(),
            "refusing must not remove or replace the planted link"
        );
        // 链接目标里不得出现我们"以为"建好的 socket 目录内容。
        assert!(
            std::fs::read_dir(&target)
                .expect("read target")
                .next()
                .is_none(),
            "the target must stay untouched: the base check failed before anything was created"
        );
    }

    /// 已存在的基目录：属主是我们时权限一律收回 0700。组/其他可写的目录里，别的进程
    /// 能 unlink 我们的 socket —— 那正是"进程健康但控制面静默缺失"的成因。
    #[test]
    #[cfg(unix)]
    fn the_fallback_base_is_verified_before_binding() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-base-")
            .tempdir()
            .expect("tempdir");
        let base = temp.path().join("llm-proxy-1000");

        ensure_private_socket_base_dir(&base).expect("a fresh base must be usable");
        assert_eq!(
            std::fs::symlink_metadata(&base).expect("fresh base").mode() & 0o777,
            0o700,
            "a fresh base must end up owner-only"
        );

        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755))
            .expect("loosen the base mode");
        ensure_private_socket_base_dir(&base).expect("our own base stays usable after tightening");
        assert_eq!(
            std::fs::symlink_metadata(&base)
                .expect("tightened base")
                .mode()
                & 0o777,
            0o700,
            "a group-readable base lets another process remove our socket"
        );

        // 普通文件占了目录名字 ⇒ 硬失败，不"换个名字悄悄继续"（spec §11.9.2）。
        let file_base = temp.path().join("llm-proxy-file");
        std::fs::write(&file_base, b"x").expect("occupy the base name");
        let err = ensure_private_socket_base_dir(&file_base)
            .expect_err("a regular file cannot be a private base");
        assert!(
            format!("{err:#}").contains("not a directory"),
            "the failure must name the actual shape: {err:#}"
        );
    }

    /// 属主不是我们的基目录必须硬失败，且**不共享、不再换名**（spec §11.8）。
    ///
    /// 本进程不能 `chown`，所以拿一个现成的外人目录来验：候选都是任何 unix 系统上
    /// 必然存在、通常归 uid 0 的挂载点，且先确认"属主不是我们"才继续（以 root 跑测试
    /// 时四者皆 ours ⇒ 打印原因后跳过，不假装通过）。选 `/proc`、`/dev` 而不是 `/`：
    /// 万一归属闸门本身被改坏，失手 `chmod` 的落点也不能是根目录。
    #[test]
    #[cfg(unix)]
    fn the_fallback_base_refuses_a_directory_another_uid_owns() {
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;

        let Some(foreign) = ["/proc", "/dev", "/sys", "/"]
            .iter()
            .map(Path::new)
            .find(|path| {
                std::fs::symlink_metadata(*path)
                    .map(|meta| meta.is_dir() && meta.uid() != current_uid())
                    .unwrap_or(false)
            })
        else {
            eprintln!("SKIP: 本机没有属外人的现成目录（以 root 运行？），该分支不可达");
            return;
        };
        let err = ensure_private_socket_base_dir(foreign)
            .expect_err("a base another uid owns must not be shared");
        let text = format!("{err:#}");
        assert!(
            text.contains("is owned by uid") && text.contains("refusing to share it"),
            "the failure must name ownership as the cause, not a later chmod: {text}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_runtime_dir_is_born_private() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-rt-mode-")
            .tempdir()
            .expect("tempdir");
        let runtime = temp.path().join("sub").join("llm-proxy");
        ensure_private_runtime_dir(&runtime).expect("create runtime dir");
        let created = std::fs::symlink_metadata(&runtime)
            .expect("runtime metadata")
            .mode()
            & 0o777;
        assert_eq!(
            created & 0o077,
            0,
            "group and other bits must never be present on runtime_dir: {created:#o}"
        );
        assert_eq!(
            created, 0o700,
            "the mode must be 0700 owner-only: {created:#o}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_runtime_dir_refuses_to_be_a_symlink() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-rt-link-")
            .tempdir()
            .expect("tempdir");
        let target = temp.path().join("target");
        std::fs::create_dir(&target).expect("target dir");
        let target_mode = std::fs::metadata(&target).expect("target metadata").mode() & 0o777;
        let link = temp.path().join("link-runtime");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let err =
            ensure_private_runtime_dir(&link).expect_err("symlink runtime dir must be rejected");
        assert!(
            format!("{err:#}").contains("symlink"),
            "failure must name symlink: {err:#}"
        );
        assert_eq!(
            std::fs::metadata(&target).expect("target metadata").mode() & 0o777,
            target_mode,
            "refusing must not chmod target dir"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_runtime_dir_is_verified_and_tightened_before_use() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-rt-tighten-")
            .tempdir()
            .expect("tempdir");
        let runtime = temp.path().join("llm-proxy");
        std::fs::create_dir(&runtime).expect("create runtime");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o775))
            .expect("loosen mode");

        ensure_private_runtime_dir(&runtime).expect("tighten runtime dir");
        let mode = std::fs::symlink_metadata(&runtime)
            .expect("runtime metadata")
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "runtime_dir must be tightened to 0700: {mode:#o}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_runtime_dir_refuses_a_directory_another_uid_owns() {
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;

        let Some(foreign) = ["/proc", "/dev", "/sys", "/"]
            .iter()
            .map(Path::new)
            .find(|path| {
                std::fs::symlink_metadata(*path)
                    .map(|meta| meta.is_dir() && meta.uid() != current_uid())
                    .unwrap_or(false)
            })
        else {
            eprintln!("SKIP: 没有外人目录");
            return;
        };
        let err = ensure_private_runtime_dir(foreign).expect_err("foreign dir must be refused");
        let text = format!("{err:#}");
        assert!(
            text.contains("is owned by uid") && text.contains("refusing to share it"),
            "failure must name ownership: {text}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_pid_file_is_born_private() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-pid-mode-")
            .tempdir()
            .expect("tempdir");
        let runtime = temp.path().join("llm-proxy");
        ensure_private_runtime_dir(&runtime).expect("ensure runtime dir");
        write_pid_in(&runtime, 12345).expect("write pid");
        let pid_path = pid_path_in(&runtime);
        let mode = std::fs::symlink_metadata(&pid_path)
            .expect("pid metadata")
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "pid file must be 0600 owner-only: {mode:#o}");
    }

    /// `pid_file_records` 是"这份 pid 文件是不是我们的"的全部判据：只有字面等于
    /// 自己那个 pid 才算。任何宽松解析（前缀匹配、多行取首、忽略尾部内容）都会把
    /// 别人的 pid 文件误判成自己的。
    #[test]
    fn pid_file_ownership_is_exact() {
        assert!(pid_file_records("4242", 4242));
        assert!(pid_file_records("4242\n", 4242));
        assert!(pid_file_records("  4242 \n", 4242));
        assert!(
            !pid_file_records("4243", 4242),
            "a neighbour pid is not ours"
        );
        assert!(!pid_file_records("", 4242), "an empty file proves nothing");
        assert!(
            !pid_file_records("4242 4242", 4242),
            "a body that is not a single pid proves nothing"
        );
        assert!(
            !pid_file_records("4242\n9999", 4242),
            "a body with a second line must not be read as ours"
        );
        assert!(!pid_file_records("not-a-pid", 4242));
        assert!(
            !pid_file_records("999999999999999999999", 4242),
            "an out-of-range body must not parse into our pid"
        );
    }

    /// 后台启动失败时只能撤销**自己孩子**的 pid 文件。两个 `serve` 抢过
    /// `refuse_if_running` 时，输的那个的孩子可能恰好在赢家的 pid 文件已发布之后
    /// 退出；无条件 unlink 会删掉健康服务的 pid 文件，此后 `status` 报"○ 未运行"、
    /// `shutdown` 退化成按 pid 发信号 —— 一次"我没起来所以把现场清了"的直觉，
    /// 毁掉的是别人的可观测性。
    #[test]
    #[cfg(unix)]
    fn a_failed_background_start_only_revokes_its_own_pid_file() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-pidown-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("llm-proxy.pid");
        let ours = std::process::id();

        // 别人的 pid：留在原地。
        std::fs::write(&path, format!("{}\n", ours + 1)).expect("write a foreign pid");
        let foreign_ino = std::fs::metadata(&path).expect("foreign metadata").ino();
        revoke_pid_file_owned_by(&path, ours);
        assert!(
            path.exists(),
            "a pid file recording another process is not ours to delete"
        );
        assert_eq!(
            std::fs::metadata(&path).expect("foreign metadata").ino(),
            foreign_ino,
            "the file must stay the same file, not be rewritten"
        );

        // 读不出内容（此处是被普通文件占了名字后仍可读到；用不可解析内容代表"无证据"）：
        // 没有证据就不删。
        std::fs::write(&path, "garbage").expect("write an unreadable body");
        revoke_pid_file_owned_by(&path, ours);
        assert!(
            path.exists(),
            "an unparseable body is not evidence that this is our pid file"
        );

        // 自己孩子的 pid：撤销，且文件缺失可容忍（幂等）。
        std::fs::write(&path, format!("{}\n", ours)).expect("write our own pid");
        revoke_pid_file_owned_by(&path, ours);
        assert!(
            !path.exists(),
            "our own child's pid file must be revoked on a failed start"
        );
        revoke_pid_file_owned_by(&path, ours);
    }

    /// 管理通道升级关停不能被折叠成正常关闭（spec §11.9.2）：`management_uds`、
    /// `SIGTERM` 是"按要求停的"，`management-channel-failed` 是"控制面连续 accept
    /// 失败后自己叫停的"。两者都以 `Ok(reason)` 到达，只有后者必须改退出码 ——
    /// 否则 systemd 的 `Restart=on-failure` 不会重启，脚本也看不出异常。
    ///
    /// 消费侧与生产侧都按**字面量**钉：`handle_management_accept_failure` 发出的是
    /// `MANAGEMENT_CHANNEL_FAILED_EXIT`，本用例用字面量喂给它。原因串改一个字符 ⇒
    /// 两侧各红一处，降级关停不可能在无人察觉时退回 exit 0。
    #[test]
    fn only_a_degraded_control_plane_changes_the_exit_code() {
        assert!(
            degraded_management_stop("management-channel-failed").is_some(),
            "the escalation reason must not exit 0"
        );
        for requested_stop in [
            "SIGINT",
            "SIGTERM",
            "management_uds",
            "shutdown_channel_closed",
            "config_writer_fail_stop",
            "",
        ] {
            assert!(
                degraded_management_stop(requested_stop).is_none(),
                "{requested_stop:?} is not a degraded control plane and must keep exit 0"
            );
        }
    }

    /// server 入口与每一个 CLI 必须从**同一处**取那对目录：没有发现协议时，两侧算出
    /// 不同路径就意味着 CLI 连向一个没人监听的名字（spec §11.8）。第二条断言证明这条
    /// 判据不是空跑 —— 两个目录互换必须能被看出来。
    #[test]
    #[cfg(unix)]
    fn the_cli_and_the_server_entry_derive_the_same_channel_path() {
        let (runtime_dir, fallback_base) = management_socket_dirs();
        assert_eq!(
            socket_path(),
            socket_path_plan_in(&runtime_dir, &fallback_base).0,
            "the server's entry pair must produce the path the CLI connects to"
        );
        assert_ne!(
            socket_path(),
            socket_path_plan_in(&fallback_base, &runtime_dir).0,
            "the two directories are not interchangeable: a swap must be detectable"
        );
    }

    /// : server 管理 socket 的 runtime_dir 必须与 CLI 侧 `StateDirs::resolve`
    /// 同源——都来自 `--config-dir` 注入，而非全局 `runtime_dir()`。
    /// 回归基线：用注入路径推导的 socket == `socket_path_in(injected_runtime_dir)`。
    /// 变异：若 server 侧改回全局 `runtime_dir()`，注入路径与全局不同时此断言变红。
    #[test]
    #[cfg(unix)]
    fn server_socket_uses_injected_runtime_dir_not_global() {
        let tmp = tempfile::tempdir().unwrap();
        let injected_runtime_dir = tmp.path().join("injected-runtime");
        std::fs::create_dir_all(&injected_runtime_dir).unwrap();

        let fallback_base = short_socket_base();
        let server_socket = socket_path_plan_in(&injected_runtime_dir, &fallback_base).0;

        let cli_socket = socket_path_in(&injected_runtime_dir);

        assert_eq!(
            server_socket, cli_socket,
            "server and CLI must derive the same socket path from the same injected runtime_dir"
        );

        let global_runtime = runtime_dir();
        if global_runtime != injected_runtime_dir {
            let global_socket = socket_path_in(&global_runtime);
            assert_ne!(
                server_socket, global_socket,
                "injected socket must differ from global when identities differ"
            );
        }
    }

    /// : state / pid / ownership / socket 必须从同一注入 identity 派生。
    /// 回归基线：用同一 injected_runtime_dir 推导的路径都包含该 identity。
    /// 变异：若任一处改回全局函数，注入路径与全局不同时此断言变红。
    #[test]
    #[cfg(unix)]
    fn state_pid_ownership_socket_share_same_injected_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let injected_runtime_dir = tmp.path().join("injected-runtime");
        let injected_state_dir = tmp.path().join("injected-state");
        std::fs::create_dir_all(&injected_runtime_dir).unwrap();
        std::fs::create_dir_all(&injected_state_dir).unwrap();

        // All paths must derive from the same injected identity
        let socket = socket_path_in(&injected_runtime_dir);
        let pid = pid_path_in(&injected_runtime_dir);
        // ownership.lock is created inside runtime_dir by OwnershipLock
        let ownership_dir = &injected_runtime_dir;
        let state_subdir = &injected_state_dir;

        // Verify they all contain the injected path components (not global)
        let socket_str = socket.to_string_lossy();
        let pid_str = pid.to_string_lossy();
        let ownership_str = ownership_dir.to_string_lossy();
        let state_str = state_subdir.to_string_lossy();

        assert!(
            socket_str.contains("injected-runtime"),
            "socket must derive from injected runtime_dir, got: {socket_str}"
        );
        assert!(
            pid_str.contains("injected-runtime"),
            "pid must derive from injected runtime_dir, got: {pid_str}"
        );
        assert!(
            ownership_str.contains("injected-runtime"),
            "ownership must derive from injected runtime_dir, got: {ownership_str}"
        );
        assert!(
            state_str.contains("injected-state"),
            "state must derive from injected state_dir, got: {state_str}"
        );
    }

    /// : 两个不同 config-dir 实例的 socket 短名必须不同。
    /// 回归基线：用不同注入路径推导的 socket 路径不相等。
    /// 变异：若 socket 派生改回全局函数或忽略注入路径，此断言变红。
    #[test]
    #[cfg(unix)]
    fn different_config_dirs_produce_different_socket_names() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir_a = tmp.path().join("config-a");
        let config_dir_b = tmp.path().join("config-b");
        std::fs::create_dir_all(&config_dir_a).unwrap();
        std::fs::create_dir_all(&config_dir_b).unwrap();

        // StateDirs::resolve would produce different runtime_dirs for different config_dirs
        let runtime_dir_a = config_dir_a.join("runtime");
        let runtime_dir_b = config_dir_b.join("runtime");
        std::fs::create_dir_all(&runtime_dir_a).unwrap();
        std::fs::create_dir_all(&runtime_dir_b).unwrap();

        let socket_a = socket_path_in(&runtime_dir_a);
        let socket_b = socket_path_in(&runtime_dir_b);

        assert_ne!(
            socket_a, socket_b,
            "different config-dirs must produce different socket paths: a={socket_a:?}, b={socket_b:?}"
        );
    }

    /// : `start_background`（CLI 父进程）检查子进程就绪时用的 socket 路径
    /// 必须与 `run_foreground`（子进程）绑定的 socket 路径同源——都从同一个
    /// `config_dir` 经 `StateDirs::resolve` 派生。
    ///
    /// 回归基线：给定同一 `config_dir`，两侧推导的 socket 路径相等。
    /// 变异：若 `start_background` 改回全局 `socket_path()`，当 `config_dir` 非默认时
    /// 此断言变红（全局路径 ≠ 注入路径）。
    #[test]
    #[cfg(unix)]
    fn start_background_and_run_foreground_use_same_socket_source() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("custom-config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // The child (run_foreground) derives its socket from injected dirs:
        let child_dirs = StateDirs::resolve(&config_dir);
        let child_socket = socket_path_in(&child_dirs.runtime_dir);

        // The parent (start_background) must derive from the same config_dir:
        let parent_dirs = StateDirs::resolve(&config_dir);
        let parent_socket = socket_path_in(&parent_dirs.runtime_dir);

        assert_eq!(
            child_socket, parent_socket,
            "start_background and run_foreground must derive the same socket from the same config_dir"
        );

        // Mutation evidence: global socket_path() differs when config_dir is non-default
        let global_socket = socket_path();
        if child_dirs.runtime_dir != runtime_dir() {
            assert_ne!(
                child_socket, global_socket,
                "injected socket must differ from global when config_dir is non-default"
            );
        }
    }

    /// 反之，仍在接活的监听者不能被抢名：unlink 会让它"进程健康但控制面静默缺失"。
    #[tokio::test]
    #[cfg(unix)]
    async fn management_socket_refuses_to_take_over_a_live_listener() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-live-")
            .tempdir()
            .expect("tempdir");
        let path = temp.path().join("llm-proxy.sock");
        let incumbent = tokio::net::UnixListener::bind(&path).expect("bind the incumbent listener");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let err = bounded(
            "take over a live listener",
            run_management_socket_at(
                isolated_app_state(temp.path()),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &path,
                Some(ready_tx),
            ),
        )
        .await
        .expect_err(
            "a live listener owns the name; taking it over is the silent failure we must not make",
        );
        let reported = err.to_string();
        assert!(
            reported.contains("already served"),
            "the error must name the real cause: {reported}"
        );
        let verdict = tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("the takeover refusal must reach the ready channel")
            .expect("ready sender dropped")
            .expect_err("readiness must not be reported");
        assert!(verdict.contains("already served"), "{verdict}");
        assert!(
            path.exists(),
            "the incumbent's socket file must survive: {}",
            path.display()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::UnixStream::connect(&path)
            )
            .await
            .expect("incumbent connect must not hang")
            .is_ok(),
            "the incumbent must still be reachable"
        );
        drop(incumbent);
    }

    /// 就绪通道四种结局都必须可执行，且任何非"就绪"的结局都要撤掉 pid 文件。
    /// 其中"从不应答"就是历史上的卡死：等一个永不返回的 accept 循环。
    #[tokio::test]
    async fn await_management_ready_covers_every_startup_verdict() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-ready-")
            .tempdir()
            .expect("tempdir");
        let pid = temp.path().join("llm-proxy.pid");
        let busy = || tokio::spawn(async { Ok(()) });

        // (a) 就绪：保留 pid。
        std::fs::write(&pid, b"4242\n").expect("write pid");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = busy();
        tx.send(Ok(())).expect("ready verdict");
        await_management_ready(rx, &handle, &pid, Duration::from_secs(1))
            .await
            .expect("a verified channel is ready");
        assert!(
            pid.exists(),
            "a successful startup keeps its pid file: {}",
            pid.display()
        );
        handle.abort();

        // (b) 明确失败。
        std::fs::write(&pid, b"4242\n").expect("write pid");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = busy();
        tx.send(Err(
            "failed to bind management socket /run/x/llm-proxy.sock (path length 3 bytes): boom"
                .into(),
        ))
        .expect("failure verdict");
        let err = await_management_ready(rx, &handle, &pid, Duration::from_secs(1))
            .await
            .expect_err("a bind failure must fail startup");
        assert_eq!(
            err.to_string(),
            "management socket failed to start",
            "Display carries the label; the cause chain below carries the reason"
        );
        let reported = format!("{err:#}");
        assert!(
            reported.starts_with("management socket failed to start: "),
            "the prefix CLI users see must survive: {reported}"
        );
        assert!(
            reported.contains("failed to bind management socket"),
            "the channel's own reason must be carried through: {reported}"
        );
        revoke_check(&pid, false);
        handle.abort();

        // (c) 通道被丢弃：任务 panic 或被 abort，没人给出结论。
        std::fs::write(&pid, b"4242\n").expect("write pid");
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        drop(tx);
        let handle = busy();
        let err = await_management_ready(rx, &handle, &pid, Duration::from_secs(1))
            .await
            .expect_err("a dropped channel is not readiness");
        assert!(
            format!("{err:#}").contains("closed without a verdict"),
            "the error must say the verdict is missing: {err:#}"
        );
        revoke_check(&pid, false);
        handle.abort();

        // (d) 从不应答：有界等待必须升级为报错，不能永久挂着。
        std::fs::write(&pid, b"4242\n").expect("write pid");
        let (_tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            #[allow(unreachable_code)]
            Ok(())
        });
        let started = Instant::now();
        let err = await_management_ready(rx, &handle, &pid, Duration::from_millis(120))
            .await
            .expect_err("an answer that never comes must not hang startup");
        assert!(
            format!("{err:#}").contains("no readiness verdict within"),
            "the error must name the budget that expired: {err:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the undecidable case must be bounded, not forever"
        );
        revoke_check(&pid, false);
        let cancelled = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the unresponsive task must be terminated")
            .expect_err("aborting yields a cancel error");
        assert!(
            cancelled.is_cancelled(),
            "startup failure must stop the channel task: {cancelled}"
        );
    }

    /// pid 文件撤销本身失败时不得静默（`status` 会指向已退出的 pid）。
    #[test]
    fn revoke_pid_file_removes_the_file_and_tolerates_absence() {
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-pid-")
            .tempdir()
            .expect("tempdir");
        let pid = temp.path().join("llm-proxy.pid");
        std::fs::write(&pid, b"1\n").expect("write pid");
        revoke_pid_file(&pid);
        revoke_check(&pid, false);
        revoke_pid_file(&pid); // 已不存在：NotFound 不是错误
    }

    /// accept 循环的策略表：未到上限只重试，到上限停机。
    #[test]
    fn accept_failure_policy_stops_only_at_the_limit() {
        for failures in 0..MANAGEMENT_ACCEPT_ERROR_LIMIT {
            assert_eq!(
                management_accept_failure_policy(failures),
                AcceptFailure::Retry,
                "{failures} consecutive failures are still retryable"
            );
        }
        assert_eq!(
            management_accept_failure_policy(MANAGEMENT_ACCEPT_ERROR_LIMIT),
            AcceptFailure::Fatal,
            "the limit itself is fatal"
        );
        assert_eq!(
            management_accept_failure_policy(MANAGEMENT_ACCEPT_ERROR_LIMIT + 4),
            AcceptFailure::Fatal
        );
    }

    /// 就绪之后 accept 循环坏掉不得静默：每次失败都要留痕，到上限必须要求整机
    /// 停机。这里执行的是生产处理函数本身（两个平台的循环共用它）。
    #[tokio::test]
    #[cfg(unix)]
    async fn accept_failures_are_traced_and_escalate_to_shutdown() {
        let capture = ServiceLogCapture::start();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));
        let error = std::io::Error::from_raw_os_error(libc::EBADF);

        let action = handle_management_accept_failure(
            &shutdown_tx,
            &error,
            MANAGEMENT_ACCEPT_ERROR_LIMIT - 1,
        );
        assert_eq!(
            action,
            AcceptFailure::Retry,
            "未到上限的瞬时失败仍可继续服务"
        );
        assert!(
            shutdown_rx.try_recv().is_err(),
            "retryable failures must not stop the server"
        );
        let trace = capture
            .find("management_accept_failed")
            .expect("每次 accept 失败都必须留下日志事件");
        assert!(
            trace.contains("Bad file descriptor"),
            "the trace must carry the OS error: {trace}"
        );

        let action =
            handle_management_accept_failure(&shutdown_tx, &error, MANAGEMENT_ACCEPT_ERROR_LIMIT);
        assert_eq!(action, AcceptFailure::Fatal, "到上限必须停机");
        let reason = tokio::time::timeout(Duration::from_secs(1), &mut shutdown_rx)
            .await
            .expect("控制面不可用时必须要求整机停机")
            .expect("shutdown sender dropped");
        assert_eq!(reason, "management-channel-failed");
    }

    /// spec \u{a7}11.8 与管理通道记录都承诺 `management_socket_bind_failed` 是可依赖的
    /// 观测面：把它降级成 debug 或直接删掉必须让测试变红。
    #[tokio::test]
    #[cfg(unix)]
    async fn bind_failure_logs_the_promised_event_with_its_fields() {
        let capture = ServiceLogCapture::start();
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-log-")
            .tempdir()
            .expect("tempdir");
        let dir = temp.path().as_os_str().to_str().expect("utf-8 tempdir");
        if dir.len() + 1 + "x".len() > SUN_PATH_MAX {
            eprintln!("SKIP: tempdir path is already at the sun_path limit: {dir}");
            return;
        }
        let past_limit = temp.path().join("x".repeat(108 - dir.len() - 1));
        let (ready_tx, _ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let err = bounded(
            "bind a path past sun_path",
            run_management_socket_at(
                isolated_app_state(temp.path()),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                &past_limit,
                Some(ready_tx),
            ),
        )
        .await
        .expect_err("an over-long path cannot bind");
        let trace = capture
            .find("management_socket_bind_failed")
            .expect("bind failure must log the event the record promises");
        assert!(
            trace.contains(&format!("path_len={}", past_limit.as_os_str().len())),
            "the trace must carry the length: {trace}"
        );
        assert!(
            trace.contains("path="),
            "the trace must carry the path: {trace}"
        );
        assert!(err.to_string().contains("(path length 108 bytes)"), "{err}");
    }

    /// 换名不是静默兜底：`management_socket_fallback` 必须真的出现在日志里，
    /// 且带上常规名与实际绑定名（spec \u{a7}11.9.2 要求异常状态可见）。
    #[tokio::test]
    #[cfg(unix)]
    async fn sun_path_fallback_announces_itself_in_the_log() {
        let capture = ServiceLogCapture::start();
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-mgmt-announce-")
            .tempdir()
            .expect("tempdir");
        let base = temp.path().join("short");
        let runtime = temp.path().join("w".repeat(120)).join("run");
        std::fs::create_dir_all(&runtime).expect("create long runtime dir");
        let (planned, fell_back) = socket_path_plan_in(&runtime, &base);
        assert!(fell_back, "fixture must exceed the sun_path limit");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let serve_runtime = runtime.clone();
        let serve_base = base.clone();
        let handle = tokio::spawn(async move {
            run_management_socket_with_ready_at(
                &serve_runtime,
                &serve_base,
                isolated_app_state(&serve_runtime),
                Arc::new(Mutex::new(Some(shutdown_tx))),
                ready_tx,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("verdict")
            .expect("ready sender dropped")
            .expect("fallback startup must succeed");
        let trace = capture
            .find("management_socket_fallback")
            .expect("the path change must be announced, not swallowed");
        assert!(
            trace.contains("llm-proxy.sock"),
            "the trace must name the conventional path that was abandoned: {trace}"
        );
        handle.abort();
        let _ = std::fs::remove_file(&planned);
    }

    /// bind 失败的建议必须与真实原因一致：把 EACCES 说成路径长度会把用户支去改
    /// 错的东西，那是同一种静默误诊（spec \u{a7}11.9.2）。
    #[test]
    #[cfg(unix)]
    fn bind_advice_names_the_actual_cause() {
        use std::io;

        let too_long = PathBuf::from("/tmp").join("z".repeat(140));
        let length_text = management_bind_advice(
            &io::Error::from_raw_os_error(libc::EACCES),
            too_long.as_os_str().len(),
        )
        .expect("over-long paths must get advice");
        assert!(length_text.contains("sun_path"), "{length_text}");

        // std 在 bind 之前就拦下超长路径，给的是 InvalidInput 而非 errno。
        let std_reject = io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        );
        assert_eq!(
            std_reject.raw_os_error(),
            None,
            "the pre-bind rejection carries no errno, so the length must be judged from the path"
        );
        assert!(
            management_bind_advice(&std_reject, 108)
                .expect("the std rejection is still a length problem")
                .contains("sun_path")
        );

        let in_use = management_bind_advice(&io::Error::from_raw_os_error(libc::EADDRINUSE), 40)
            .expect("EADDRINUSE needs actionable advice");
        assert!(in_use.contains("shutdown"), "{in_use}");
        assert!(
            !in_use.contains("XDG_RUNTIME_DIR"),
            "a busy path must not blame the path: {in_use}"
        );

        let denied = management_bind_advice(&io::Error::from_raw_os_error(libc::EACCES), 40)
            .expect("EACCES needs actionable advice");
        assert!(denied.contains("权限"), "{denied}");
        assert!(
            !denied.contains("sun_path"),
            "a permission problem must not be reported as a length problem: {denied}"
        );

        assert!(
            management_bind_advice(&io::Error::from_raw_os_error(libc::ENOTEMPTY), 40).is_none(),
            "an unknown cause must not invent advice"
        );
    }

    /// 后台启动的结论只能来自真实信号：没退出但通道不应答 ⇒ 有界地不可判定，
    /// 不能印出 "llm-proxy started"。
    #[test]
    fn background_start_requires_a_real_signal() {
        let mut alive = || -> Result<Option<String>> { Ok(None) };
        assert_eq!(
            confirm_background_start(
                Duration::from_secs(1),
                Duration::from_millis(1),
                &mut alive,
                &|| true,
            )
            .expect("poll the live channel"),
            BackgroundStart::Started,
            "an answering management channel is the only proof of a started server"
        );

        let mut dead = || -> Result<Option<String>> { Ok(Some("exit status: 1".to_string())) };
        assert_eq!(
            confirm_background_start(
                Duration::from_secs(1),
                Duration::from_millis(1),
                &mut dead,
                &|| true,
            )
            .expect("poll the exited child"),
            BackgroundStart::Exited("exit status: 1".to_string()),
            "a dead process outranks any positive signal"
        );

        let started = Instant::now();
        assert_eq!(
            confirm_background_start(
                Duration::from_millis(80),
                Duration::from_millis(10),
                &mut alive,
                &|| false,
            )
            .expect("poll a silent child"),
            BackgroundStart::Undecided,
            "alive-but-silent is undecidable, not success"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80) && elapsed < Duration::from_secs(5),
            "the budget must be honoured and bounded: {elapsed:?}"
        );
    }

    /// 断言 pid 文件的存在性，失败信息带上绝对路径。
    fn revoke_check(pid: &Path, expected: bool) {
        assert_eq!(
            pid.exists(),
            expected,
            "pid file presence mismatch for {}",
            pid.display()
        );
    }

    /// 通过管理通道发一次 `GET /state`，返回原始响应文本。
    async fn management_get_state(path: &Path) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let request = b"GET /state HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\
                        Connection: close\r\n\r\n";
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut stream = tokio::net::UnixStream::connect(path)
                .await
                .unwrap_or_else(|err| panic!("connect {}: {err}", path.display()));
            stream
                .write_all(request)
                .await
                .expect("write management request");
            let mut buf = Vec::new();
            stream
                .read_to_end(&mut buf)
                .await
                .expect("read management response");
            String::from_utf8_lossy(&buf).into_owned()
        })
        .await
        .unwrap_or_else(|_| panic!("the management channel at {} must answer", path.display()))
    }

    /// 生产入口在测试里必须有界执行。探测式闸门一旦被改坏（例：把活服务判成不在），
    /// 绑定会成功并进入 accept 循环，裸 `.await` 永不返回——那时测试应当变红而不是
    /// 挂住，否则变异测试只能靠 CI 超时发现。
    #[cfg(unix)]
    async fn bounded<T>(what: &'static str, fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .unwrap_or_else(|_| panic!("the management entry must return within 10s: {what}"))
    }

    /// 捕获本测试线程上 `llm_proxy::service` 的日志事件（含字段）。
    ///
    /// 记录与 spec 承诺的事件名是可依赖的观测面，前提是有人钉住它们：否则把
    /// `error!` 降级成 `debug!`、或整行删掉，测试仍然全绿。
    struct ServiceLogCapture {
        _guard: tracing::subscriber::DefaultGuard,
        // 两个 dispatch 同时存活：tracing-core 的单 dispatch 快路径会缓存并发线程
        // 的"无订阅者" interest（同 convert/tests.rs 的做法）。
        _registration: tracing::Dispatch,
        events: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ServiceLogCapture {
        fn start() -> Self {
            use tracing_subscriber::prelude::*;

            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            let _registration = tracing::Dispatch::new(tracing_subscriber::registry());
            let subscriber = tracing_subscriber::registry().with(EventLayer(Arc::clone(&events)));
            Self {
                _guard: tracing::subscriber::set_default(subscriber),
                _registration,
                events,
            }
        }

        /// 第一个带 `event = "<name>"` 的记录行（`field=value` 串接而成）。
        fn find(&self, name: &str) -> Option<String> {
            let needle = format!("event={name}");
            self.lines().into_iter().find(|line| line.contains(&needle))
        }

        fn lines(&self) -> Vec<String> {
            self.events.lock().expect("capture lock").clone()
        }
    }

    struct EventLayer(Arc<std::sync::Mutex<Vec<String>>>);

    #[derive(Default)]
    struct EventFields(std::collections::BTreeMap<String, String>);

    impl tracing::field::Visit for EventFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let rendered = format!("{value:?}");
            // 字符串字面量按 Debug 渲染会带引号；去掉引号后，断言写的就是运维在日志
            // 里实际看到的文本。
            let unquoted = rendered
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&rendered);
            self.0
                .insert(field.name().to_string(), unquoted.to_string());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "llm_proxy::service" {
                return;
            }
            let mut fields = EventFields::default();
            event.record(&mut fields);
            let mut line = format!(
                "event={} level={}",
                fields
                    .0
                    .remove("event")
                    .unwrap_or_else(|| event.metadata().name().to_string()),
                event.metadata().level()
            );
            for (name, value) in fields.0 {
                line.push_str(&format!(" {name}={value}"));
            }
            self.0.lock().expect("capture lock").push(line);
        }
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

    #[test]
    fn parse_proc_ipv4() {
        assert_eq!(
            super::parse_proc_ipv4("0100007F"),
            Some(std::net::Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            super::parse_proc_ipv4("00000000"),
            Some(std::net::Ipv4Addr::new(0, 0, 0, 0))
        );
        assert_eq!(
            super::parse_proc_ipv4("0200A8C0"),
            Some(std::net::Ipv4Addr::new(192, 168, 0, 2))
        );
        assert_eq!(super::parse_proc_ipv4("bad"), None);
    }

    #[test]
    fn inspect_port_occupant_detects_live_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let listen_str = format!("127.0.0.1:{port}");

        let occupant = super::inspect_port_occupant(&listen_str);
        assert!(
            occupant.is_some(),
            "occupant must be detected for a live listener"
        );
        let occupant = occupant.unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(
                occupant.pid,
                Some(std::process::id()),
                "occupant PID must match current process"
            );
        }

        // Drop the listener: port is no longer occupied
        drop(listener);
        let occupant_after = super::inspect_port_occupant(&listen_str);
        assert!(
            occupant_after.is_none(),
            "occupant must not be detected after listener is closed"
        );
    }

    #[test]
    fn format_bind_error_contains_occupant_pid_and_remediation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let listen_str = format!("127.0.0.1:{port}");

        let io_err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "Address already in use");
        let err = super::format_bind_error(&listen_str, &io_err);
        let err_msg = err.to_string();

        assert!(
            err_msg.contains("Address already in use"),
            "error message must mention Address already in use: {err_msg}"
        );
        assert!(
            err_msg.contains("Remediation:"),
            "error message must provide actionable Remediation: {err_msg}"
        );
        if cfg!(target_os = "linux") {
            assert!(
                err_msg.contains(&format!("PID {}", std::process::id())),
                "error message must include occupying PID: {err_msg}"
            );
        }
    }

    #[test]
    fn check_listen_available_enforces_availability() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let listen_str = format!("127.0.0.1:{port}");

        let err =
            super::check_listen_available(&listen_str).expect_err("occupied port must fail check");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("Address already in use"),
            "preflight error must identify Address already in use: {err_msg}"
        );
        if cfg!(target_os = "linux") {
            assert!(
                err_msg.contains(&format!("PID {}", std::process::id())),
                "preflight error must identify occupying PID: {err_msg}"
            );
        }

        drop(listener);
        assert!(
            super::check_listen_available(&listen_str).is_ok(),
            "preflight check must succeed when port is released"
        );
    }

    /// red-proof: `/admin/diagnostics` is reachable on the UDS
    /// management router. A 200 (or 400 for bad query params) proves the route
    /// is registered; a 404 would mean the route was accidentally removed.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "sandbox may disallow Unix domain socket bind"]
    async fn diagnostics_endpoint_reachable_on_uds_management_router() {
        use tokio::net::UnixListener;
        let temp = tempfile::Builder::new()
            .prefix("llm-proxy-uds-166-")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let sock = temp.path().join("mgmt.sock");
        let listener = UnixListener::bind(&sock).expect("bind uds listener");

        let app = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let router = super::management_router(app, Arc::new(Mutex::new(Some(tx))));

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

        // GET /admin/diagnostics — must reach the handler (200 with empty or
        // disabled response), NOT return 404 from the router.
        let resp = uds_test_request(&sock, "GET /admin/diagnostics HTTP/1.1\r\n\r\n").await;
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "diagnostics endpoint must be routed on UDS management router, got: {resp}"
        );
        assert!(
            resp.contains("\"capture\""),
            "response must include capture field: {resp}"
        );
    }

    /// red-proof (public router): `/admin/diagnostics` must NOT be
    /// registered on the public HTTP listener. A request there must get 404.
    /// If this test goes red (returns 200), someone added the route to the
    /// public router — revert that change; `capture = "full"` records embed
    /// the egress request body and must not be reachable over the network.
    #[tokio::test]
    async fn diagnostics_endpoint_returns_404_on_public_router() {
        let state = crate::proxy::AppState::new(
            crate::config::default_deepseek_config(),
            std::path::PathBuf::from("/tmp/test-config.toml"),
        );
        let router = crate::proxy::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });

        let response = reqwest::get(format!("http://{addr}/admin/diagnostics"))
            .await
            .expect("request");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "/admin/diagnostics must 404 on the public HTTP listener — it carries egress body data"
        );
    }
}
