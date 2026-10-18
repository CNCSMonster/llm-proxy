use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot, watch};

use crate::config::Config;
use crate::config_render::semantic_config_bytes;

const TERMINAL_RESULT_CAPACITY: usize = 256;
const TERMINAL_RESULT_TTL: Duration = Duration::from_secs(600);
/// 等待关闭结论的默认预算（「不可判定必须区别于成功」）。
///
/// actor 正常收尾是毫秒级；这里给到 30s 是为了不在"慢但成功"的关闭上误报不可判定，
/// 同时保证"结论永远不产出"不会变成永久挂起。
const DEFAULT_SHUTDOWN_RESULT_BUDGET: Duration = Duration::from_secs(30);

/// : writer ack 等待预算。
///
/// writer 正常处理 commit/mutate/reload 是毫秒级；给到 5s 是为了覆盖慢磁盘写入，
/// 同时保证"writer 收了消息但不回 ack"不会变成永久挂起（启动卡死）。
/// 测试里通过 `set_commit_ack_timeout_for_test` 改小来确定性触发超时。
const DEFAULT_COMMIT_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const RUNNING: u8 = 0;
const CLOSING: u8 = 1;
const CLOSED: u8 = 2;
const FAILED_STOP: u8 = 3;

/// actor 退出时向 supervisor 报告的自身结论。
///
/// actor 改为"只报告，不判决"——不再写 `lifecycle` 终态值，
/// 由 supervisor 根据 `ActorExit` + join 错误唯一判定终态。
enum ActorExit {
    Clean,
    FailedStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ClientOperationKey(pub(crate) u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OperationId {
    pub(crate) boot_epoch: u128,
    pub(crate) sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperationReceipt {
    pub(crate) key: ClientOperationKey,
    pub(crate) id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriterError {
    ServiceShuttingDown,
    MailboxClosed,
    RetrySafetyUnknown,
    OperationKeyConflict,
    MutationRejected(String),
    FailedBeforeReplace(String),
    ReloadFailed(String),
    CommitFailed(String),
    /// (D3 form B): the commit's base revision no longer matches the
    /// actor's current revision; the commit was rejected fail-stop.
    CommitBaseConflict {
        expected: u64,
        actual: u64,
    },
    RevisionExhausted,
    FailedStop,
    /// 「不可判定必须区别于成功」：在预算内没有拿到关闭终态。
    /// 它**既不是** `Ok(())`（干净关闭）**也不是** `FailedStop`（已确认的 fail-stop），
    /// 必须作为第三种结论对外暴露，禁止被折算成其中任何一个。
    StopUndetermined,
    OperationExpired,
    UnknownOperation,
    /// : writer accepted the message (send succeeded) but the ack
    /// never arrived within the budget. Distinct from `RetrySafetyUnknown`
    /// (sender dropped) and `MailboxClosed` (channel gone) — the writer may
    /// still be processing; the caller must not assume success or failure.
    AckTimeout,
}

impl std::str::FromStr for WriterError {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "operation_expired" => Ok(Self::OperationExpired),
            "unknown_operation" => Ok(Self::UnknownOperation),
            "service_shutting_down" => Ok(Self::ServiceShuttingDown),
            "mailbox_closed" => Ok(Self::MailboxClosed),
            "retry_safety_unknown" => Ok(Self::RetrySafetyUnknown),
            "operation_key_conflict" => Ok(Self::OperationKeyConflict),
            "revision_exhausted" => Ok(Self::RevisionExhausted),
            "failed_stop" => Ok(Self::FailedStop),
            "stop_undetermined" => Ok(Self::StopUndetermined),
            "ack_timeout" => Ok(Self::AckTimeout),
            other => Ok(Self::CommitFailed(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Commit,
    Mutate,
    Reload,
}

impl OperationKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Mutate => "mutate",
            Self::Reload => "reload",
        }
    }
}

#[derive(Clone)]
pub(crate) struct OperationIdentity {
    kind: OperationKind,
    digest: [u8; 32],
    canonical: Arc<[u8]>,
}

#[cfg(test)]
impl OperationIdentity {
    pub(crate) fn test_identity(kind: OperationKind, tag: &[u8]) -> Self {
        let mut digest = [0u8; 32];
        let len = tag.len().min(32);
        digest[..len].copy_from_slice(&tag[..len]);
        Self {
            kind,
            digest,
            canonical: Arc::from(tag),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum TerminalResponse {
    Commit { revision: u64 },
    Mutate { revision: u64, message: String },
    Reload { config: Box<Config> },
    Error(WriterError),
}

impl PartialEq for TerminalResponse {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Commit { revision: r1 }, Self::Commit { revision: r2 }) => r1 == r2,
            (
                Self::Mutate {
                    revision: r1,
                    message: m1,
                },
                Self::Mutate {
                    revision: r2,
                    message: m2,
                },
            ) => r1 == r2 && m1 == m2,
            (Self::Reload { config: c1 }, Self::Reload { config: c2 }) => {
                semantic_config_bytes(c1).ok() == semantic_config_bytes(c2).ok()
            }
            (Self::Error(e1), Self::Error(e2)) => e1 == e2,
            _ => false,
        }
    }
}
impl Eq for TerminalResponse {}

struct AdmittedOperation {
    receipt: OperationReceipt,
    identity: OperationIdentity,
    terminal_at: Option<Instant>,
    terminal: Option<TerminalResponse>,
}

struct Tombstone {
    key: ClientOperationKey,
    id: OperationId,
    evicted_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) enum AdmissionCheck {
    Allow,
    Replay {
        receipt: OperationReceipt,
        terminal: TerminalResponse,
    },
    Reject(WriterError),
}

#[derive(Default)]
pub(crate) struct RetentionStore {
    operations: HashMap<OperationId, AdmittedOperation>,
    by_key: HashMap<ClientOperationKey, OperationId>,
    terminal_order: VecDeque<OperationId>,
    tombstones: HashMap<ClientOperationKey, OperationId>,
    tombstones_by_id: HashMap<OperationId, ClientOperationKey>,
    tombstone_order: VecDeque<Tombstone>,
}

impl RetentionStore {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        while let Some(id) = self.terminal_order.front().copied() {
            let expired = self.terminal_order.len() > TERMINAL_RESULT_CAPACITY
                || self
                    .operations
                    .get(&id)
                    .and_then(|op| op.terminal_at)
                    .and_then(|at| now.checked_duration_since(at))
                    .is_some_and(|d| d >= TERMINAL_RESULT_TTL);
            if !expired {
                break;
            }
            self.terminal_order.pop_front();
            if let Some(operation) = self.operations.remove(&id) {
                self.by_key.remove(&operation.receipt.key);
                self.tombstones
                    .insert(operation.receipt.key, operation.receipt.id);
                self.tombstones_by_id
                    .insert(operation.receipt.id, operation.receipt.key);
                self.tombstone_order.push_back(Tombstone {
                    key: operation.receipt.key,
                    id: operation.receipt.id,
                    evicted_at: now,
                });
            }
        }
        while let Some(tombstone) = self.tombstone_order.front() {
            let expired = self.tombstone_order.len() > TERMINAL_RESULT_CAPACITY
                || now
                    .checked_duration_since(tombstone.evicted_at)
                    .is_some_and(|d| d >= TERMINAL_RESULT_TTL);
            if !expired {
                break;
            }
            let tombstone = self.tombstone_order.pop_front().expect("front exists");
            self.tombstones.remove(&tombstone.key);
            self.tombstones_by_id.remove(&tombstone.id);
        }
    }

    pub(crate) fn check(
        &self,
        key: ClientOperationKey,
        identity: &OperationIdentity,
    ) -> AdmissionCheck {
        if self.tombstones.contains_key(&key) {
            return AdmissionCheck::Reject(WriterError::RetrySafetyUnknown);
        }
        if let Some(id) = self.by_key.get(&key).copied() {
            let Some(operation) = self.operations.get(&id) else {
                return AdmissionCheck::Reject(WriterError::RetrySafetyUnknown);
            };
            if operation.identity.kind != identity.kind
                || operation.identity.digest != identity.digest
                || operation.identity.canonical.as_ref() != identity.canonical.as_ref()
            {
                AdmissionCheck::Reject(WriterError::OperationKeyConflict)
            } else if let Some(terminal) = &operation.terminal {
                AdmissionCheck::Replay {
                    receipt: operation.receipt,
                    terminal: terminal.clone(),
                }
            } else {
                AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
            }
        } else {
            AdmissionCheck::Allow
        }
    }

    #[cfg(test)]
    pub(crate) fn lookup(
        &self,
        receipt: OperationReceipt,
        boot_epoch: u128,
    ) -> Result<TerminalResponse, WriterError> {
        if receipt.id.boot_epoch != boot_epoch {
            return Err(WriterError::UnknownOperation);
        }
        if let Some(operation) = self.operations.get(&receipt.id) {
            if operation.receipt.key != receipt.key {
                return Err(WriterError::UnknownOperation);
            }
            if let Some(terminal) = &operation.terminal {
                return Ok(terminal.clone());
            } else {
                return Err(WriterError::RetrySafetyUnknown);
            }
        }
        if self.tombstones_by_id.get(&receipt.id) == Some(&receipt.key) {
            return Err(WriterError::OperationExpired);
        }
        Err(WriterError::UnknownOperation)
    }

    pub(crate) fn admit(&mut self, receipt: OperationReceipt, identity: OperationIdentity) {
        self.by_key.insert(receipt.key, receipt.id);
        self.operations.insert(
            receipt.id,
            AdmittedOperation {
                receipt,
                identity,
                terminal_at: None,
                terminal: None,
            },
        );
    }

    pub(crate) fn register_terminal(
        &mut self,
        id: OperationId,
        terminal: TerminalResponse,
        now: Instant,
    ) {
        if let Some(operation) = self.operations.get_mut(&id) {
            operation.terminal_at = Some(now);
            operation.terminal = Some(terminal);
            self.terminal_order.push_back(id);
            self.expire(now);
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_len(&self) -> usize {
        self.terminal_order.len()
    }

    #[cfg(test)]
    pub(crate) fn operations_len(&self) -> usize {
        self.operations.len()
    }

    #[cfg(test)]
    pub(crate) fn tombstones_len(&self) -> usize {
        self.tombstones.len()
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &ClientOperationKey) -> bool {
        self.by_key.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn contains_tombstone(&self, key: &ClientOperationKey) -> bool {
        self.tombstones.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn get_terminal_at(&self, id: &OperationId) -> Option<Instant> {
        self.operations.get(id).and_then(|op| op.terminal_at)
    }

    #[cfg(test)]
    pub(crate) fn terminal_order(&self) -> Vec<OperationId> {
        self.terminal_order.iter().copied().collect()
    }
}

#[derive(Clone)]
struct FixedScope {
    config_path: PathBuf,
    state_dir: PathBuf,
    config_path_identity: String,
    state_dir_identity: String,
}

impl FixedScope {
    fn new(config_path: PathBuf, state_dir: PathBuf) -> Self {
        let config_path = normalize_path(&config_path);
        let state_dir = normalize_path(&state_dir);
        Self {
            config_path_identity: config_path.display().to_string(),
            state_dir_identity: state_dir.display().to_string(),
            config_path,
            state_dir,
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    if path.exists() {
        return std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    match path.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    }
}

fn mutation_payload(mutation: &crate::model::ConfigMutation) -> serde_json::Value {
    use crate::model::ConfigMutation;
    match mutation {
        ConfigMutation::Add {
            model_id,
            context_window,
            max_output_tokens,
            copy_from,
            routes,
        } => serde_json::json!({
            "variant": "add", "model_id": model_id, "context_window": context_window,
            "max_output_tokens": max_output_tokens, "copy_from": copy_from,
            "routes": routes.as_ref().map(|r| r.iter().map(|e| format!("{}={}", e.level, e.target)).collect::<Vec<_>>()),
        }),
        ConfigMutation::Set { model_id, options } => serde_json::json!({
            "variant": "set", "model_id": model_id,
            "context_window": options.context_window,
            "max_output_tokens": options.max_output_tokens,
            "reasoning_levels": options
                .reasoning_levels
                .as_ref()
                .map(crate::model::ReasoningLevelsUpdate::digest),
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
        ConfigMutation::Remove { model_id, force } => serde_json::json!({
            "variant": "remove", "model_id": model_id, "force": force,
        }),
        ConfigMutation::Rename {
            old_model,
            new_model,
        } => serde_json::json!({
            "variant": "rename", "old_model": old_model, "new_model": new_model,
        }),
        ConfigMutation::ProviderAdd {
            model_id,
            protocol,
            provider_id,
            upstream_model,
        } => serde_json::json!({
            "variant": "provider_add", "model_id": model_id,
            "protocol": { "type": "Protocol", "value": protocol.field_name() },
            "provider_id": provider_id, "upstream_model": upstream_model,
        }),
        ConfigMutation::ProviderSet {
            model_id,
            protocol,
            provider_id,
            upstream_model,
        } => serde_json::json!({
            "variant": "provider_set", "model_id": model_id,
            "protocol": { "type": "Protocol", "value": protocol.field_name() },
            "provider_id": provider_id, "upstream_model": upstream_model,
        }),
        ConfigMutation::ProviderRemove {
            model_id,
            protocol,
            provider_id,
        } => serde_json::json!({
            "variant": "provider_remove", "model_id": model_id,
            "protocol": { "type": "Protocol", "value": protocol.field_name() },
            "provider_id": provider_id,
        }),
        ConfigMutation::ProviderMove {
            model_id,
            protocol,
            provider_id,
            to_index,
        } => serde_json::json!({
            "variant": "provider_move", "model_id": model_id,
            "protocol": { "type": "Protocol", "value": protocol.field_name() },
            "provider_id": provider_id, "to_index": to_index,
        }),
    }
}

fn identity(
    kind: OperationKind,
    payload: serde_json::Value,
    scope: &FixedScope,
) -> Result<OperationIdentity, WriterError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": "v1",
        "operation_kind": kind.tag(),
        "payload": payload,
        "config_path": scope.config_path_identity,
        "state_scope": scope.state_dir_identity,
    }))
    .map_err(|error| WriterError::CommitFailed(error.to_string()))?;
    let digest = Sha256::digest(&payload).into();
    Ok(OperationIdentity {
        kind,
        digest,
        canonical: Arc::from(payload),
    })
}

fn commit_identity(config: &Config, scope: &FixedScope) -> Result<OperationIdentity, WriterError> {
    let bytes = semantic_config_bytes(config).map_err(WriterError::CommitFailed)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| WriterError::CommitFailed(error.to_string()))?;
    identity(OperationKind::Commit, value, scope)
}
fn mutate_identity(
    mutation: &crate::model::ConfigMutation,
    scope: &FixedScope,
) -> Result<OperationIdentity, WriterError> {
    identity(OperationKind::Mutate, mutation_payload(mutation), scope)
}
fn reload_identity(scope: &FixedScope) -> Result<OperationIdentity, WriterError> {
    identity(
        OperationKind::Reload,
        serde_json::json!({ "source": "fixed_scope" }),
        scope,
    )
}

#[derive(Clone)]
pub(crate) struct FatalConfigState {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Default for FatalConfigState {
    fn default() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }
}

impl FatalConfigState {
    pub(crate) fn is_failed(&self) -> bool {
        *self.rx.borrow()
    }
    pub(crate) fn fail(&self) {
        let _ = self.tx.send(true);
    }
    pub(crate) async fn wait(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow_and_update() {
            return;
        }
        let _ = rx.wait_for(|failed| *failed).await;
    }
}

enum WriterMessage {
    Commit {
        key: ClientOperationKey,
        config: Box<Config>,
        base: u64,
        ack: oneshot::Sender<Result<(OperationReceipt, u64), WriterError>>,
    },
    Mutate {
        key: ClientOperationKey,
        mutation: crate::model::ConfigMutation,
        ack: oneshot::Sender<Result<(OperationReceipt, u64, String), WriterError>>,
    },
    Reload {
        key: ClientOperationKey,
        ack: oneshot::Sender<Result<(OperationReceipt, Config), WriterError>>,
    },
    #[cfg(test)]
    Lookup {
        receipt: OperationReceipt,
        ack: oneshot::Sender<Result<TerminalResponse, WriterError>>,
    },
    BeginShutdown {
        ack: oneshot::Sender<()>,
    },
}

#[cfg(test)]
impl WriterMessage {
    fn operation_key(&self) -> Option<ClientOperationKey> {
        match self {
            WriterMessage::Commit { key, .. }
            | WriterMessage::Mutate { key, .. }
            | WriterMessage::Reload { key, .. } => Some(*key),
            WriterMessage::Lookup { receipt, .. } => Some(receipt.key),
            WriterMessage::BeginShutdown { .. } => None,
        }
    }
}

#[cfg(test)]
type ShutdownTestBarrier = Arc<TokioMutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ShutdownTestHook {
    barrier: ShutdownTestBarrier,
    marker_barrier: Arc<TokioMutex<Option<oneshot::Sender<()>>>>,
    send_pending_barrier: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

#[cfg(test)]
impl ShutdownTestHook {
    pub(crate) fn new(started_tx: oneshot::Sender<()>, resume_rx: oneshot::Receiver<()>) -> Self {
        Self {
            barrier: Arc::new(TokioMutex::new(Some((started_tx, resume_rx)))),
            marker_barrier: Arc::new(TokioMutex::new(None)),
            send_pending_barrier: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn new_with_marker(
        started_tx: oneshot::Sender<()>,
        resume_rx: oneshot::Receiver<()>,
        marker_tx: oneshot::Sender<()>,
    ) -> Self {
        Self {
            barrier: Arc::new(TokioMutex::new(Some((started_tx, resume_rx)))),
            marker_barrier: Arc::new(TokioMutex::new(Some(marker_tx))),
            send_pending_barrier: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn new_with_marker_and_send_pending(
        started_tx: oneshot::Sender<()>,
        resume_rx: oneshot::Receiver<()>,
        marker_tx: oneshot::Sender<()>,
        send_pending_tx: oneshot::Sender<()>,
    ) -> Self {
        Self {
            barrier: Arc::new(TokioMutex::new(Some((started_tx, resume_rx)))),
            marker_barrier: Arc::new(TokioMutex::new(Some(marker_tx))),
            send_pending_barrier: Arc::new(std::sync::Mutex::new(Some(send_pending_tx))),
        }
    }

    async fn pause_before_send(&self) {
        let pair = {
            let mut guard = self.barrier.lock().await;
            guard.take()
        };
        if let Some((started_tx, resume_rx)) = pair {
            let _ = started_tx.send(());
            let _ = resume_rx.await;
        }
    }

    async fn on_marker_sent(&self) {
        let sender = {
            let mut guard = self.marker_barrier.lock().await;
            guard.take()
        };
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
    }

    fn on_send_polled_pending(&self) {
        let sender = {
            let mut guard = self.send_pending_barrier.lock().unwrap();
            guard.take()
        };
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
    }
}

struct ShutdownCoordinator {
    tx: mpsc::Sender<WriterMessage>,
    lifecycle: Arc<AtomicU8>,
    #[allow(dead_code)] // read via fatal_config_state() method
    fatal: FatalConfigState,
    coordination_task: TokioMutex<Option<tokio::task::JoinHandle<()>>>,
    // : supervisor 返回 Result<ActorExit, JoinError>
    actor_task:
        TokioMutex<Option<tokio::task::JoinHandle<Result<ActorExit, tokio::task::JoinError>>>>,
    shared_result_rx: watch::Receiver<Option<Result<(), WriterError>>>,
    /// 结论等待预算（毫秒）。用原子量而不是 `#[cfg(test)]` 常量：测试与生产必须走
    /// 完全相同的代码形状，cfg 分叉正是躲过全部单测的原因。
    result_budget_ms: AtomicU64,
    #[cfg(test)]
    test_hook: Option<ShutdownTestHook>,
    #[cfg(test)]
    phase_hook: Option<PhaseHook>,
}

impl ShutdownCoordinator {
    fn new(
        tx: mpsc::Sender<WriterMessage>,
        lifecycle: Arc<AtomicU8>,
        fatal: FatalConfigState,
        actor_task: tokio::task::JoinHandle<Result<ActorExit, tokio::task::JoinError>>,
        shared_result_rx: watch::Receiver<Option<Result<(), WriterError>>>,
        #[cfg(test)] test_hooks: (Option<ShutdownTestHook>, Option<PhaseHook>),
    ) -> Self {
        #[cfg(test)]
        let (test_hook, phase_hook) = test_hooks;
        Self {
            tx,
            lifecycle,
            fatal,
            coordination_task: TokioMutex::new(None),
            actor_task: TokioMutex::new(Some(actor_task)),
            shared_result_rx,
            result_budget_ms: AtomicU64::new(DEFAULT_SHUTDOWN_RESULT_BUDGET.as_millis() as u64),
            #[cfg(test)]
            test_hook,
            #[cfg(test)]
            phase_hook,
        }
    }

    fn inactive(
        tx: mpsc::Sender<WriterMessage>,
        lifecycle: Arc<AtomicU8>,
        fatal: FatalConfigState,
    ) -> Self {
        let (_shared_result_tx, shared_result_rx) = watch::channel(Some(Ok(())));
        Self {
            tx,
            lifecycle,
            fatal,
            coordination_task: TokioMutex::new(None),
            actor_task: TokioMutex::new(None),
            shared_result_rx,
            result_budget_ms: AtomicU64::new(DEFAULT_SHUTDOWN_RESULT_BUDGET.as_millis() as u64),
            #[cfg(test)]
            test_hook: None,
            #[cfg(test)]
            phase_hook: None,
        }
    }

    pub(crate) async fn execute(&self) -> Result<(), WriterError> {
        if let Some(res) = &*self.shared_result_rx.borrow() {
            return res.clone();
        }

        {
            let mut coord_guard = self.coordination_task.lock().await;
            if let Some(res) = &*self.shared_result_rx.borrow() {
                return res.clone();
            }

            // : 不再在此处编造结论。若 lifecycle 已到 FAILED_STOP，
            // supervisor 已判定/将判定终态并写入通道——直接跳到下方有界等待。
            // 仅当尚未派生协调任务且未失败时才派生（避免向已死 actor 发 BeginShutdown）。
            if coord_guard.is_none() && self.lifecycle.load(Ordering::Acquire) != FAILED_STOP {
                let actor_task_opt = {
                    let mut guard = self.actor_task.lock().await;
                    guard.take()
                };
                let tx = self.tx.clone();
                #[cfg(test)]
                let test_hook = self.test_hook.clone();
                #[cfg(test)]
                let phase_hook = self.phase_hook.clone();

                let handle = tokio::spawn(async move {
                    // : 协调任务降级为"发起 + 观察"。
                    // 只负责发 BeginShutdown、等 ack；不再 send 结论、不再写 lifecycle。
                    #[cfg(test)]
                    if let Some(hook) = &phase_hook {
                        hook.on_phase(
                            OperationId {
                                boot_epoch: 0,
                                sequence: 0,
                            },
                            OperationPhase::BeforeBeginShutdown,
                        )
                        .await;
                    }

                    #[cfg(test)]
                    if let Some(hook) = &test_hook {
                        hook.pause_before_send().await;
                    }

                    let mut join_fut = std::pin::pin!(async {
                        if let Some(task) = actor_task_opt {
                            task.await
                        } else {
                            Ok(Ok(ActorExit::Clean))
                        }
                    });

                    // supervisor 任务的 join 结果**只允许取一次**。
                    // `select!` 会真的 poll 每个分支里的 `&mut join_fut`：一旦某一轮把它
                    // poll 到 Ready 却没走对应的 await，之后再 await 就是对已完成 future
                    // 的二次 poll ⇒ `` `async fn` resumed after completion `` panic
                    // 所以 Ready 时立刻把值取进 `join_taken`，后续一律复用缓存。
                    // 注：`select!` 只取第一个 Ready 的分支，故 join 被 poll 到 Ready 时必然
                    // 走它的分支 ⇒ 缓存不会被覆盖，也不会再被 poll。
                    #[allow(clippy::type_complexity)]
                    let mut join_taken: Option<
                        Result<Result<ActorExit, tokio::task::JoinError>, tokio::task::JoinError>,
                    > = None;

                    let (ack_tx, ack_rx) = oneshot::channel();
                    #[cfg(not(test))]
                    let send_fut = tx.send(WriterMessage::BeginShutdown { ack: ack_tx });
                    #[cfg(test)]
                    let send_fut = {
                        let mut inner =
                            Box::pin(tx.send(WriterMessage::BeginShutdown { ack: ack_tx }));
                        let hook = test_hook.clone();
                        std::future::poll_fn(move |cx| {
                            let poll_res = std::future::Future::poll(inner.as_mut(), cx);
                            if poll_res.is_pending()
                                && let Some(hook) = &hook
                            {
                                hook.on_send_polled_pending();
                            }
                            poll_res
                        })
                    };
                    let send_ok = tokio::select! {
                        send_res = send_fut => send_res.is_ok(),
                        join_res = &mut join_fut => {
                            join_taken = Some(join_res);
                            false
                        }
                    };
                    #[cfg(test)]
                    if send_ok && let Some(hook) = &test_hook {
                        hook.on_marker_sent().await;
                    }
                    if send_ok {
                        // 测试与生产共用同一个 select 形状：曾经分叉写法正是
                        // 躲过全部单测的原因（生产分支单测驱动不到）。
                        #[cfg_attr(not(test), allow(unused_variables))]
                        let _ack_ok = tokio::select! {
                            ack_res = ack_rx => ack_res.is_ok(),
                            join_res = &mut join_fut => {
                                join_taken = Some(join_res);
                                false
                            }
                        };
                        #[cfg(test)]
                        if _ack_ok && let Some(hook) = &phase_hook {
                            hook.on_phase(
                                OperationId {
                                    boot_epoch: 0,
                                    sequence: 0,
                                },
                                OperationPhase::ShutdownAckReceived,
                            )
                            .await;
                        }
                    }

                    // : 协调任务不再判定终态、不再写通道。
                    // join_fut / join_taken 在此 drop——supervisor 独立运行，
                    // 结论由 supervisor 唯一写入 watch 通道，execute() 有界读取。
                    drop(join_taken);
                });
                *coord_guard = Some(handle);
            }
        }

        let mut rx = self.shared_result_rx.clone();
        if let Some(res) = rx.borrow_and_update().clone() {
            return res;
        }
        let budget = Duration::from_millis(self.result_budget_ms.load(Ordering::Acquire));
        // 先绑定再 match：否则 `timeout(..)` 的临时值会把 `&mut rx` 一直持有到整个
        // match 结束，超时分支里就没法再复查通道。
        let waited = tokio::time::timeout(budget, rx.wait_for(|res| res.is_some())).await;
        match waited {
            Ok(Ok(val)) => val.as_ref().unwrap().clone(),
            Ok(Err(_)) => {
                if self.lifecycle.load(Ordering::Acquire) == FAILED_STOP {
                    Err(WriterError::FailedStop)
                } else {
                    Err(WriterError::MailboxClosed)
                }
            }
            Err(_elapsed) => {
                // 超时先复查一遍：结论可能就在预算到期的同一拍刚落通道，这时报"不可判定"
                // 是假警报。终态已确认的两种情况也照实上报，不折算成第三种。
                if let Some(res) = self.shared_result_rx.borrow().clone() {
                    return res;
                }
                if self.lifecycle.load(Ordering::Acquire) == FAILED_STOP {
                    return Err(WriterError::FailedStop);
                }
                // 到这里才是真正取不到结论：既不假装成功，也不冒充 fail-stop。
                Err(WriterError::StopUndetermined)
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ConfigWriterHandle {
    tx: mpsc::Sender<WriterMessage>,
    lifecycle: Arc<AtomicU8>,
    /// : writer ack 等待预算（毫秒）。与 `ShutdownCoordinator::result_budget_ms`
    /// 同构——用原子量而不是 `#[cfg(test)]` 常量，保证测试与生产走完全相同的代码形状。
    commit_ack_timeout_ms: Arc<AtomicU64>,
    #[cfg(test)]
    fatal: FatalConfigState,
    coordinator: Arc<ShutdownCoordinator>,
    #[cfg(test)]
    phase_hook: Option<PhaseHook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplaceOutcome {
    NotReplaced(String),
    Replaced,
    Unknown(String),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OperationPhase {
    Admitted,
    Prepared,
    BeforeReplace,
    AfterReplace,
    AfterLoad,
    BeforeCorePublish,
    BeforeGenerationPublish,
    Published,
    BeforeAck,
    BeforeBeginShutdown,
    Closing,
    Drained,
    BeforeActorExit,
    ShutdownAckReceived,
}

#[cfg(test)]
pub(crate) struct PhasePauseHandle {
    reached_rx: oneshot::Receiver<()>,
    resume_tx: Option<oneshot::Sender<()>>,
}

#[cfg(test)]
impl PhasePauseHandle {
    pub(crate) async fn wait_reached(&mut self) {
        let _ = (&mut self.reached_rx).await;
    }

    pub(crate) fn release(mut self) {
        if let Some(tx) = self.resume_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct PhaseHook {
    inner: Arc<std::sync::Mutex<PhaseHookInner>>,
}

#[cfg(test)]
#[derive(Default)]
struct PhaseHookInner {
    history: Vec<(OperationId, OperationPhase)>,
    pauses: HashMap<(u64, OperationPhase), (oneshot::Sender<()>, oneshot::Receiver<()>)>,
    listeners: Vec<(u64, OperationPhase, Option<oneshot::Sender<()>>)>,
    panics: HashMap<(u64, OperationPhase), String>,
    enqueued: Vec<ClientOperationKey>,
    enqueue_listeners: Vec<(ClientOperationKey, Option<oneshot::Sender<()>>)>,
}

#[cfg(test)]
#[allow(dead_code)]
impl PhaseHook {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn panic_on(
        &self,
        sequence: u64,
        phase: OperationPhase,
        message: impl Into<String>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.panics.insert((sequence, phase), message.into());
    }

    pub(crate) fn pause(&self, sequence: u64, phase: OperationPhase) -> PhasePauseHandle {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap();
        inner
            .pauses
            .insert((sequence, phase), (reached_tx, resume_rx));
        PhasePauseHandle {
            reached_rx,
            resume_tx: Some(resume_tx),
        }
    }

    pub(crate) fn on_enqueued(&self, key: ClientOperationKey) {
        let mut inner = self.inner.lock().unwrap();
        inner.enqueued.push(key);
        inner.enqueue_listeners.retain_mut(|(expected_key, tx)| {
            if *expected_key == key {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
                false
            } else {
                true
            }
        });
    }

    pub(crate) async fn wait_enqueued(&self, key: ClientOperationKey) {
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            if inner.enqueued.contains(&key) {
                return;
            }
            let (tx, rx) = oneshot::channel();
            inner.enqueue_listeners.push((key, Some(tx)));
            rx
        };
        let _ = rx.await;
    }

    pub(crate) async fn wait_phase(&self, sequence: u64, phase: OperationPhase) {
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            if inner
                .history
                .iter()
                .any(|(id, p)| (sequence == 0 || id.sequence == sequence) && *p == phase)
            {
                return;
            }
            let (tx, rx) = oneshot::channel();
            inner.listeners.push((sequence, phase, Some(tx)));
            rx
        };
        let _ = rx.await;
    }

    pub(crate) fn enqueued_count(&self, key: ClientOperationKey) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.enqueued.iter().filter(|&&k| k == key).count()
    }

    pub(crate) async fn on_phase(&self, id: OperationId, phase: OperationPhase) {
        let (panic_msg, pause_to_await) = {
            let mut inner = self.inner.lock().unwrap();
            inner.history.push((id, phase));
            inner.listeners.retain_mut(|(seq, p, tx)| {
                if (*seq == id.sequence || *seq == 0) && *p == phase {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(());
                    }
                    false
                } else {
                    true
                }
            });
            let panic_msg = inner
                .panics
                .remove(&(id.sequence, phase))
                .or_else(|| inner.panics.remove(&(0, phase)));
            let pause = inner
                .pauses
                .remove(&(id.sequence, phase))
                .or_else(|| inner.pauses.remove(&(0, phase)));
            (panic_msg, pause)
        };

        if let Some(msg) = panic_msg {
            panic!("{msg}");
        }

        if let Some((reached_tx, resume_rx)) = pause_to_await {
            let _ = reached_tx.send(());
            let _ = resume_rx.await;
        }
    }

    pub(crate) fn phase_count(&self, sequence: u64, phase: OperationPhase) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .history
            .iter()
            .filter(|(id, p)| (sequence == 0 || id.sequence == sequence) && *p == phase)
            .count()
    }

    pub(crate) fn history(&self) -> Vec<(OperationId, OperationPhase)> {
        self.inner.lock().unwrap().history.clone()
    }
}

#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TestPersistenceOutcome {
    Replaced,
    NotReplaced(String),
    Unknown(String),
    PostReplacementUnknown(String),
    PostReplacementPanic(String),
    PostReplacementJoinError(String),
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) type TestReplaceOutcome = TestPersistenceOutcome;

#[cfg(test)]
impl From<ReplaceOutcome> for TestPersistenceOutcome {
    fn from(outcome: ReplaceOutcome) -> Self {
        match outcome {
            ReplaceOutcome::Replaced => Self::Replaced,
            ReplaceOutcome::NotReplaced(error) => Self::NotReplaced(error),
            ReplaceOutcome::Unknown(error) => Self::Unknown(error),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestPersistence {
    inner: Arc<std::sync::Mutex<TestPersistenceInner>>,
}

#[cfg(test)]
#[derive(Default)]
struct TestPersistenceInner {
    load_calls: usize,
    replace_calls: usize,
    recorded_bytes: Vec<Vec<u8>>,
    queued_outcomes: VecDeque<TestPersistenceOutcome>,
    default_outcome: Option<TestPersistenceOutcome>,
}

#[cfg(test)]
#[allow(dead_code)]
impl TestPersistence {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_default_outcome(&self, outcome: impl Into<TestPersistenceOutcome>) {
        self.inner.lock().unwrap().default_outcome = Some(outcome.into());
    }

    pub(crate) fn queue_outcome(&self, outcome: impl Into<TestPersistenceOutcome>) {
        self.inner
            .lock()
            .unwrap()
            .queued_outcomes
            .push_back(outcome.into());
    }

    pub(crate) fn panic_after_replacement(&self, message: impl Into<String>) {
        self.queue_outcome(TestPersistenceOutcome::PostReplacementPanic(message.into()));
    }

    pub(crate) fn call_count(&self) -> usize {
        self.inner.lock().unwrap().replace_calls
    }

    /// Counts writer loads only, excluding direct fixture setup/validation reads.
    pub(crate) fn load_call_count(&self) -> usize {
        self.inner.lock().unwrap().load_calls
    }

    pub(crate) fn recorded_bytes(&self) -> Vec<Vec<u8>> {
        self.inner.lock().unwrap().recorded_bytes.clone()
    }

    pub(crate) fn execute(
        &self,
        config_path: &Path,
        state_dir: &Path,
        bytes: Vec<u8>,
    ) -> ReplaceOutcome {
        let outcome = {
            let mut inner = self.inner.lock().unwrap();
            inner.replace_calls += 1;
            inner.recorded_bytes.push(bytes.clone());
            inner
                .queued_outcomes
                .pop_front()
                .or_else(|| inner.default_outcome.clone())
        };
        match outcome {
            None => write_replacement(config_path, state_dir, bytes),
            Some(TestPersistenceOutcome::Replaced) => {
                let _ = write_replacement(config_path, state_dir, bytes);
                ReplaceOutcome::Replaced
            }
            Some(TestPersistenceOutcome::PostReplacementUnknown(error)) => {
                let _ = write_replacement(config_path, state_dir, bytes);
                ReplaceOutcome::Unknown(error)
            }
            Some(TestPersistenceOutcome::PostReplacementPanic(message))
            | Some(TestPersistenceOutcome::PostReplacementJoinError(message)) => {
                let _ = write_replacement(config_path, state_dir, bytes);
                panic!("{message}");
            }
            Some(TestPersistenceOutcome::NotReplaced(error)) => ReplaceOutcome::NotReplaced(error),
            Some(TestPersistenceOutcome::Unknown(error)) => ReplaceOutcome::Unknown(error),
        }
    }
}

fn write_replacement(path: &Path, state_dir: &Path, bytes: Vec<u8>) -> ReplaceOutcome {
    let lock = match crate::core::ConfigLock::acquire(state_dir, Duration::from_secs(5)) {
        Ok(lock) => lock,
        Err(error) => return ReplaceOutcome::NotReplaced(error.to_string()),
    };
    let _lock = lock;
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return ReplaceOutcome::NotReplaced(error.to_string());
    }
    let tmp = path.with_extension(format!(
        "{}.runtime-config.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("toml")
    ));
    if let Err(error) = std::fs::write(&tmp, bytes) {
        return ReplaceOutcome::NotReplaced(error.to_string());
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => ReplaceOutcome::Replaced,
        // A failed rename is deliberately treated as uncertain. The OS error
        // alone is not our proof that the target was not replaced.
        Err(error) => ReplaceOutcome::Unknown(error.to_string()),
    }
}

async fn load_config(
    path: PathBuf,
    state_dir: PathBuf,
    #[cfg(test)] persistence: Option<&TestPersistence>,
) -> Result<Config, WriterError> {
    #[cfg(test)]
    let persistence = persistence.cloned();

    let result = tokio::task::spawn_blocking(move || {
        let _lock = crate::core::ConfigLock::acquire(&state_dir, Duration::from_secs(5))?;
        #[cfg(test)]
        if let Some(persistence) = persistence {
            persistence.inner.lock().unwrap().load_calls += 1;
        }
        Config::load(&path)
    })
    .await;
    match result {
        Ok(Ok(config)) => Ok(config),
        Ok(Err(error)) => Err(WriterError::ReloadFailed(error.to_string())),
        Err(_error) => Err(WriterError::RetrySafetyUnknown),
    }
}

async fn persist_config(
    scope: FixedScope,
    bytes: Vec<u8>,
    #[cfg(test)] persistence: Option<&TestPersistence>,
) -> ReplaceOutcome {
    #[cfg(test)]
    let persistence = persistence.cloned();

    match tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if let Some(p) = persistence {
            return p.execute(&scope.config_path, &scope.state_dir, bytes);
        }
        write_replacement(&scope.config_path, &scope.state_dir, bytes)
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => ReplaceOutcome::Unknown(format!("config replacement task failed: {error}")),
    }
}

async fn publish_runtime_config(
    core: &Arc<tokio::sync::Mutex<crate::core::CoreState>>,
    generation: &GenerationReader,
    config: Config,
    revision: u64,
    #[cfg(test)] phase_hook: Option<&PhaseHook>,
    #[cfg(test)] op_id: OperationId,
) {
    #[cfg(test)]
    if let Some(hook) = phase_hook {
        hook.on_phase(op_id, OperationPhase::BeforeCorePublish)
            .await;
    }

    // No await occurs while the compatibility mirror is held.
    {
        let mut core = core.lock().await;
        core.replace_config_in_memory(config.clone());
    }

    #[cfg(test)]
    if let Some(hook) = phase_hook {
        hook.on_phase(op_id, OperationPhase::BeforeGenerationPublish)
            .await;
    }

    generation.publish(config, revision);

    #[cfg(test)]
    if let Some(hook) = phase_hook {
        hook.on_phase(op_id, OperationPhase::Published).await;
    }
}

#[cfg(test)]
#[derive(Default, Clone)]
pub(crate) struct WriterTestControls {
    pub(crate) test_hook: Option<ShutdownTestHook>,
    pub(crate) phase_hook: Option<PhaseHook>,
    pub(crate) test_persistence: Option<TestPersistence>,
}

#[cfg(test)]
impl WriterTestControls {
    pub(crate) fn with_hook(hook: Option<ShutdownTestHook>) -> Self {
        Self {
            test_hook: hook,
            phase_hook: None,
            test_persistence: None,
        }
    }

    pub(crate) fn with_phase_and_persistence(
        phase_hook: Option<PhaseHook>,
        test_persistence: Option<TestPersistence>,
    ) -> Self {
        Self {
            test_hook: None,
            phase_hook,
            test_persistence,
        }
    }
}

impl ConfigWriterHandle {
    pub(crate) fn inactive() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let lifecycle = Arc::new(AtomicU8::new(CLOSED));
        let fatal = FatalConfigState::default();
        let coordinator = Arc::new(ShutdownCoordinator::inactive(
            tx.clone(),
            lifecycle.clone(),
            fatal.clone(),
        ));
        Self {
            tx,
            lifecycle,
            commit_ack_timeout_ms: Arc::new(AtomicU64::new(
                DEFAULT_COMMIT_ACK_TIMEOUT.as_millis() as u64
            )),
            #[cfg(test)]
            fatal,
            coordinator,
            #[cfg(test)]
            phase_hook: None,
        }
    }

    pub(crate) fn start_for_current_runtime(
        boot_epoch: u128,
        capacity: usize,
        generation: GenerationReader,
        core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        config_path: PathBuf,
        state_dir: PathBuf,
        fatal: FatalConfigState,
    ) -> (Self, Option<tokio::task::JoinHandle<()>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return (Self::inactive(), None);
        }
        let (handle, task) = Self::start(
            boot_epoch,
            capacity,
            generation,
            core,
            config_path,
            state_dir,
            fatal,
        );
        (handle, Some(task))
    }

    pub(crate) fn start(
        boot_epoch: u128,
        capacity: usize,
        generation: GenerationReader,
        core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        config_path: PathBuf,
        state_dir: PathBuf,
        fatal: FatalConfigState,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        Self::start_internal(
            boot_epoch,
            capacity,
            generation,
            core,
            FixedScope::new(config_path, state_dir),
            fatal,
            #[cfg(test)]
            WriterTestControls::default(),
        )
    }

    #[cfg(test)]
    fn start_with_hook(
        boot_epoch: u128,
        capacity: usize,
        generation: GenerationReader,
        core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        scope: FixedScope,
        fatal: FatalConfigState,
        hook: Option<ShutdownTestHook>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        Self::start_internal(
            boot_epoch,
            capacity,
            generation,
            core,
            scope,
            fatal,
            WriterTestControls::with_hook(hook),
        )
    }

    #[cfg(test)]
    fn start_with_controls(
        boot_epoch: u128,
        capacity: usize,
        generation: GenerationReader,
        core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        scope: FixedScope,
        fatal: FatalConfigState,
        controls: WriterTestControls,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        Self::start_internal(
            boot_epoch, capacity, generation, core, scope, fatal, controls,
        )
    }

    fn start_internal(
        boot_epoch: u128,
        capacity: usize,
        generation: GenerationReader,
        core: Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        scope: FixedScope,
        fatal: FatalConfigState,
        #[cfg(test)] controls: WriterTestControls,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel(capacity.max(1));
        let lifecycle = Arc::new(AtomicU8::new(RUNNING));
        let lifecycle_task = lifecycle.clone();
        let fatal_task = fatal.clone();
        #[cfg(test)]
        let phase_hook_for_handle = controls.phase_hook.clone();
        #[cfg(test)]
        let phase_hook = controls.phase_hook;
        #[cfg(test)]
        let test_persistence = controls.test_persistence;
        let (actor_exit_tx, mut actor_exit_rx) = watch::channel(None);
        let task = tokio::spawn(async move {
            let initial = generation.load();
            let mut current_config = initial.config().clone();
            let mut revision = initial.revision();
            // : actor 不再写 lifecycle 终态，改用局部标志向 supervisor 报告。
            let mut actor_had_fatal = false;
            // Verify fixed start-up injection before accepting an operation.
            let core_config = { core.lock().await.config().clone() };
            match (
                semantic_config_bytes(&current_config),
                semantic_config_bytes(&core_config),
            ) {
                (Ok(a), Ok(b)) if a == b => {}
                _ => {
                    // : 不再写 lifecycle 终态——supervisor 是唯一产出者。
                    // fatal.fail() 保留：这是"写结果未知，立刻停止服务"的即时失效，
                    // 属检测点职责，不是关闭判决。
                    fatal_task.fail();
                    rx.close();
                    while let Ok(msg) = rx.try_recv() {
                        send_error(msg, WriterError::FailedStop);
                    }
                    return ActorExit::FailedStop;
                }
            }
            let mut sequence = 0_u64;
            let mut retention = RetentionStore::default();

            while let Some(message) = rx.recv().await {
                let now = Instant::now();
                retention.expire(now);
                match message {
                    WriterMessage::BeginShutdown { ack } => {
                        if lifecycle_task.load(Ordering::Acquire) == RUNNING {
                            lifecycle_task.store(CLOSING, Ordering::Release);
                        }
                        #[cfg(test)]
                        if let Some(hook) = &phase_hook {
                            hook.on_phase(
                                OperationId {
                                    boot_epoch: 0,
                                    sequence: 0,
                                },
                                OperationPhase::Closing,
                            )
                            .await;
                        }
                        rx.close();
                        while let Some(message) = rx.recv().await {
                            reject_unadmitted(message);
                        }
                        #[cfg(test)]
                        if let Some(hook) = &phase_hook {
                            hook.on_phase(
                                OperationId {
                                    boot_epoch: 0,
                                    sequence: 0,
                                },
                                OperationPhase::Drained,
                            )
                            .await;
                        }
                        // : 不再写 CLOSED——supervisor 是唯一产出者。
                        // actor 返回 ActorExit::Clean 由 supervisor join 后判定。
                        let _ = ack.send(());
                        break;
                    }
                    #[cfg(test)]
                    WriterMessage::Lookup { receipt, ack } => {
                        if lifecycle_task.load(Ordering::Acquire) != RUNNING {
                            let _ = ack.send(Err(WriterError::ServiceShuttingDown));
                            continue;
                        }
                        let res = retention.lookup(receipt, boot_epoch);
                        let _ = ack.send(res);
                    }
                    message => {
                        if lifecycle_task.load(Ordering::Acquire) != RUNNING {
                            reject_unadmitted(message);
                            continue;
                        }
                        let (key, identity, input, ack) = unpack_operation(message, &scope);
                        let identity = match identity {
                            Ok(identity) => identity,
                            Err(error) => {
                                reply_error(ack, error);
                                continue;
                            }
                        };
                        match retention.check(key, &identity) {
                            AdmissionCheck::Reject(error) => {
                                reply_error(ack, error);
                                continue;
                            }
                            AdmissionCheck::Replay { receipt, terminal } => {
                                reply_terminal_ack(ack, receipt, &terminal);
                                continue;
                            }
                            AdmissionCheck::Allow => {}
                        }
                        sequence = match sequence.checked_add(1) {
                            Some(sequence) => sequence,
                            None => {
                                reply_error(ack, WriterError::RevisionExhausted);
                                continue;
                            }
                        };
                        let receipt = OperationReceipt {
                            key,
                            id: OperationId {
                                boot_epoch,
                                sequence,
                            },
                        };
                        retention.admit(receipt, identity);
                        #[cfg(test)]
                        if let Some(hook) = &phase_hook {
                            hook.on_phase(receipt.id, OperationPhase::Admitted).await;
                        }
                        let terminal = match input {
                            OperationInput::Commit(config, base) => {
                                if base != revision {
                                    TerminalResponse::Error(WriterError::CommitBaseConflict {
                                        expected: base,
                                        actual: revision,
                                    })
                                } else {
                                    match prepare_commit(config, revision) {
                                        Ok((candidate, next_revision, bytes)) => {
                                            #[cfg(test)]
                                            if let Some(hook) = &phase_hook {
                                                hook.on_phase(receipt.id, OperationPhase::Prepared)
                                                    .await;
                                                hook.on_phase(
                                                    receipt.id,
                                                    OperationPhase::BeforeReplace,
                                                )
                                                .await;
                                            }
                                            match persist_config(
                                                scope.clone(),
                                                bytes,
                                                #[cfg(test)]
                                                test_persistence.as_ref(),
                                            )
                                            .await
                                            {
                                                ReplaceOutcome::Replaced => {
                                                    #[cfg(test)]
                                                    if let Some(hook) = &phase_hook {
                                                        hook.on_phase(
                                                            receipt.id,
                                                            OperationPhase::AfterReplace,
                                                        )
                                                        .await;
                                                    }
                                                    publish_runtime_config(
                                                        &core,
                                                        &generation,
                                                        candidate.clone(),
                                                        next_revision,
                                                        #[cfg(test)]
                                                        phase_hook.as_ref(),
                                                        #[cfg(test)]
                                                        receipt.id,
                                                    )
                                                    .await;
                                                    current_config = candidate;
                                                    revision = next_revision;
                                                    TerminalResponse::Commit { revision }
                                                }
                                                ReplaceOutcome::NotReplaced(error) => {
                                                    TerminalResponse::Error(
                                                        WriterError::FailedBeforeReplace(error),
                                                    )
                                                }
                                                ReplaceOutcome::Unknown(_error) => {
                                                    // : 不再写 lifecycle 终态——
                                                    // 用局部标志驱动排空退出，supervisor 判定终态。
                                                    // fatal.fail() 保留：即时失效，属检测点职责。
                                                    actor_had_fatal = true;
                                                    fatal_task.fail();
                                                    TerminalResponse::Error(
                                                        WriterError::RetrySafetyUnknown,
                                                    )
                                                }
                                            }
                                        }
                                        Err(error) => TerminalResponse::Error(error),
                                    }
                                }
                            }
                            OperationInput::Mutate(mutation) => {
                                match crate::model::apply_mutation(current_config.clone(), mutation)
                                {
                                    Ok((candidate, message)) => {
                                        match prepare_commit(candidate, revision) {
                                            Ok((candidate, next_revision, bytes)) => {
                                                #[cfg(test)]
                                                if let Some(hook) = &phase_hook {
                                                    hook.on_phase(
                                                        receipt.id,
                                                        OperationPhase::Prepared,
                                                    )
                                                    .await;
                                                    hook.on_phase(
                                                        receipt.id,
                                                        OperationPhase::BeforeReplace,
                                                    )
                                                    .await;
                                                }
                                                match persist_config(
                                                    scope.clone(),
                                                    bytes,
                                                    #[cfg(test)]
                                                    test_persistence.as_ref(),
                                                )
                                                .await
                                                {
                                                    ReplaceOutcome::Replaced => {
                                                        #[cfg(test)]
                                                        if let Some(hook) = &phase_hook {
                                                            hook.on_phase(
                                                                receipt.id,
                                                                OperationPhase::AfterReplace,
                                                            )
                                                            .await;
                                                        }
                                                        publish_runtime_config(
                                                            &core,
                                                            &generation,
                                                            candidate.clone(),
                                                            next_revision,
                                                            #[cfg(test)]
                                                            phase_hook.as_ref(),
                                                            #[cfg(test)]
                                                            receipt.id,
                                                        )
                                                        .await;
                                                        current_config = candidate;
                                                        revision = next_revision;
                                                        TerminalResponse::Mutate {
                                                            revision,
                                                            message,
                                                        }
                                                    }
                                                    ReplaceOutcome::NotReplaced(error) => {
                                                        TerminalResponse::Error(
                                                            WriterError::FailedBeforeReplace(error),
                                                        )
                                                    }
                                                    ReplaceOutcome::Unknown(_error) => {
                                                        // : 同 Commit 路径——
                                                        // 局部标志替代 lifecycle 终态写入。
                                                        actor_had_fatal = true;
                                                        fatal_task.fail();
                                                        TerminalResponse::Error(
                                                            WriterError::RetrySafetyUnknown,
                                                        )
                                                    }
                                                }
                                            }
                                            Err(error) => TerminalResponse::Error(error),
                                        }
                                    }
                                    Err(error) => TerminalResponse::Error(
                                        WriterError::MutationRejected(error.to_string()),
                                    ),
                                }
                            }
                            OperationInput::Reload => match load_config(
                                scope.config_path.clone(),
                                scope.state_dir.clone(),
                                #[cfg(test)]
                                test_persistence.as_ref(),
                            )
                            .await
                            {
                                Ok(candidate) => {
                                    #[cfg(test)]
                                    if let Some(hook) = &phase_hook {
                                        hook.on_phase(receipt.id, OperationPhase::AfterLoad).await;
                                    }
                                    match revision.checked_add(1) {
                                        Some(next_revision) => {
                                            publish_runtime_config(
                                                &core,
                                                &generation,
                                                candidate.clone(),
                                                next_revision,
                                                #[cfg(test)]
                                                phase_hook.as_ref(),
                                                #[cfg(test)]
                                                receipt.id,
                                            )
                                            .await;
                                            current_config = candidate.clone();
                                            revision = next_revision;
                                            TerminalResponse::Reload {
                                                config: Box::new(candidate),
                                            }
                                        }
                                        None => {
                                            TerminalResponse::Error(WriterError::RevisionExhausted)
                                        }
                                    }
                                }
                                Err(error) => TerminalResponse::Error(error),
                            },
                        };
                        let now = Instant::now();
                        retention.register_terminal(receipt.id, terminal.clone(), now);
                        #[cfg(test)]
                        if let Some(hook) = &phase_hook {
                            hook.on_phase(receipt.id, OperationPhase::BeforeAck).await;
                        }
                        reply_terminal_ack(ack, receipt, &terminal);
                        // : 用局部标志替代 lifecycle 终态读取。
                        if actor_had_fatal {
                            rx.close();
                            while let Ok(msg) = rx.try_recv() {
                                send_error(msg, WriterError::FailedStop);
                            }
                            break;
                        }
                    }
                }
            }
            // : 不再写 lifecycle 终态——supervisor 是唯一产出者。
            // actor 只返回自身结论，由 supervisor join 后唯一判定。
            #[cfg(test)]
            if let Some(hook) = &phase_hook {
                hook.on_phase(
                    OperationId {
                        boot_epoch: 0,
                        sequence: 0,
                    },
                    OperationPhase::BeforeActorExit,
                )
                .await;
            }
            if actor_had_fatal {
                ActorExit::FailedStop
            } else {
                ActorExit::Clean
            }
        });

        let (shared_result_tx, shared_result_rx) = watch::channel(None);
        let shared_result_tx = Arc::new(shared_result_tx);
        let lifecycle_supervisor = lifecycle.clone();
        let fatal_supervisor = fatal.clone();
        let actor_exit_tx_clone = actor_exit_tx.clone();
        let shared_result_tx_clone = shared_result_tx.clone();
        let actor_abort_handle = task.abort_handle();

        let defused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let defused_clone = defused.clone();
        let actor_abort_handle_clone = actor_abort_handle.clone();
        let abort_guard = scopeguard::guard((), move |_| {
            if !defused_clone.load(Ordering::Acquire) {
                actor_abort_handle_clone.abort();
            }
        });
        let (ext_drop_tx, mut ext_drop_rx) = oneshot::channel::<()>();

        let supervisor_task = tokio::spawn(async move {
            let mut task = task;
            let join_res = tokio::select! {
                res = &mut task => res,
                _ = &mut ext_drop_rx => {
                    actor_abort_handle.abort();
                    task.await
                }
            };
            // : supervisor 是唯一产出者——根据 ActorExit + join 错误唯一判定终态。
            let is_fail = match &join_res {
                Ok(ActorExit::Clean) => false,
                Ok(ActorExit::FailedStop) => true,
                Err(_) => true,
            };
            if is_fail {
                lifecycle_supervisor.store(FAILED_STOP, Ordering::Release);
                fatal_supervisor.fail();
                let _ = shared_result_tx_clone.send(Some(Err(WriterError::FailedStop)));
            } else {
                lifecycle_supervisor.store(CLOSED, Ordering::Release);
                let _ = shared_result_tx_clone.send(Some(Ok(())));
            }
            let is_panic = match &join_res {
                Ok(_) => false,
                Err(err) => err.is_panic(),
            };
            let _ = actor_exit_tx_clone.send(Some((is_fail, is_panic)));
            join_res
        });

        let external_task = tokio::spawn(async move {
            let _abort_guard = abort_guard;
            let _drop_tx = ext_drop_tx;
            let res = actor_exit_rx.wait_for(|done| done.is_some()).await;
            if let Ok(guard) = res
                && let Some((_is_fail, is_panic)) = *guard
            {
                defused.store(true, Ordering::Release);
                if is_panic {
                    panic!("writer actor panicked unexpectedly");
                }
            }
        });

        let coordinator = Arc::new(ShutdownCoordinator::new(
            tx.clone(),
            lifecycle.clone(),
            fatal.clone(),
            supervisor_task,
            shared_result_rx,
            #[cfg(test)]
            (controls.test_hook, phase_hook_for_handle.clone()),
        ));

        (
            Self {
                tx,
                lifecycle,
                commit_ack_timeout_ms: Arc::new(AtomicU64::new(
                    DEFAULT_COMMIT_ACK_TIMEOUT.as_millis() as u64,
                )),
                #[cfg(test)]
                fatal,
                coordinator,
                #[cfg(test)]
                phase_hook: phase_hook_for_handle,
            },
            external_task,
        )
    }

    #[cfg(test)]
    pub(crate) fn fatal_config_state(&self) -> FatalConfigState {
        self.fatal.clone()
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_state(&self) -> u8 {
        self.lifecycle.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn shared_shutdown_result(&self) -> Option<Result<(), WriterError>> {
        self.coordinator.shared_result_rx.borrow().clone()
    }

    /// 把关闭结论的等待预算改小，用于在单测里确定性触发"不可判定"分支。
    /// 只改预算数值，不改 `execute()` 的控制流形状（cfg 分叉教训）。
    #[cfg(test)]
    pub(crate) fn set_shutdown_result_budget_for_test(&self, budget: Duration) {
        self.coordinator
            .result_budget_ms
            .store(budget.as_millis() as u64, Ordering::Release);
    }

    /// : 把 writer ack 等待预算改小，用于在单测里确定性触发 `AckTimeout`。
    #[cfg(test)]
    pub(crate) fn set_commit_ack_timeout_for_test(&self, budget: Duration) {
        self.commit_ack_timeout_ms
            .store(budget.as_millis() as u64, Ordering::Release);
    }

    /// 取走关闭协调任务的句柄，用于断言它**有没有 panic**。
    ///
    /// 只在测试里存在：生产路径从不等这个句柄（结论靠 watch 通道传递），所以取走它
    /// 不改变生产语义。二次 poll panic 发生在该任务内部，且 panic 会被
    /// tokio 捕获、**不会**让 `shutdown()` 返回错误（supervisor 已经在
    /// `runtime_config.rs:1808` 把结论写进通道），所以只能从任务句柄上观察到。
    #[cfg(test)]
    pub(crate) async fn take_coordination_task_for_test(
        &self,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.coordinator.coordination_task.lock().await.take()
    }

    async fn send_and_wait<T>(
        &self,
        message: WriterMessage,
        rx: oneshot::Receiver<Result<T, WriterError>>,
    ) -> Result<T, WriterError> {
        if self.lifecycle.load(Ordering::Acquire) == FAILED_STOP {
            return Err(WriterError::FailedStop);
        }
        if self.lifecycle.load(Ordering::Acquire) != RUNNING {
            return Err(WriterError::ServiceShuttingDown);
        }
        #[cfg(test)]
        let op_key = message.operation_key();
        self.tx
            .send(message)
            .await
            .map_err(|_| WriterError::MailboxClosed)?;
        #[cfg(test)]
        if let Some(key) = op_key
            && let Some(hook) = &self.phase_hook
        {
            hook.on_enqueued(key);
        }
        // : 给 ack 加预算，防止 writer 收了消息但不回 ack 时启动永久挂起。
        // 超时返回 `AckTimeout`——区别于 `RetrySafetyUnknown`（sender dropped）和
        // `MailboxClosed`（channel gone）：writer 可能仍在处理，调用方不能假设成功或失败。
        //
        // 边界说明：本修复覆盖 STARTUP commit 路径（`recover_orphan_oauth_providers`
        // → `commit_config` → `send_and_wait`）。SHUTDOWN 终态等待由 覆盖，
        // 两者共享同一根因（writer channel 上的无界 await）但场景不同。
        let budget = Duration::from_millis(self.commit_ack_timeout_ms.load(Ordering::Acquire));
        match tokio::time::timeout(budget, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_sender_dropped)) => Err(WriterError::RetrySafetyUnknown),
            Err(_elapsed) => Err(WriterError::AckTimeout),
        }
    }

    pub(crate) async fn commit(
        &self,
        key: ClientOperationKey,
        config: Config,
        base: u64,
    ) -> Result<(OperationReceipt, u64), WriterError> {
        let (tx, rx) = oneshot::channel();
        self.send_and_wait(
            WriterMessage::Commit {
                key,
                config: Box::new(config),
                base,
                ack: tx,
            },
            rx,
        )
        .await
    }
    pub(crate) async fn mutate(
        &self,
        key: ClientOperationKey,
        mutation: crate::model::ConfigMutation,
    ) -> Result<(OperationReceipt, u64, String), WriterError> {
        let (tx, rx) = oneshot::channel();
        self.send_and_wait(
            WriterMessage::Mutate {
                key,
                mutation,
                ack: tx,
            },
            rx,
        )
        .await
    }
    pub(crate) async fn reload(
        &self,
        key: ClientOperationKey,
    ) -> Result<(OperationReceipt, Config), WriterError> {
        let (tx, rx) = oneshot::channel();
        self.send_and_wait(WriterMessage::Reload { key, ack: tx }, rx)
            .await
    }
    #[cfg(test)]
    pub(crate) async fn lookup(
        &self,
        receipt: OperationReceipt,
    ) -> Result<TerminalResponse, WriterError> {
        let (tx, rx) = oneshot::channel();
        self.send_and_wait(WriterMessage::Lookup { receipt, ack: tx }, rx)
            .await
    }
    pub(crate) async fn shutdown(&self) -> Result<(), WriterError> {
        self.coordinator.execute().await
    }
}

#[allow(clippy::large_enum_variant)] // Config is passed by value everywhere in the writer; boxing here would only move the copy cost
enum OperationInput {
    Commit(Config, u64),
    Mutate(crate::model::ConfigMutation),
    Reload,
}
enum OperationAck {
    Commit(oneshot::Sender<Result<(OperationReceipt, u64), WriterError>>),
    Mutate(oneshot::Sender<Result<(OperationReceipt, u64, String), WriterError>>),
    Reload(oneshot::Sender<Result<(OperationReceipt, Config), WriterError>>),
}
fn unpack_operation(
    message: WriterMessage,
    scope: &FixedScope,
) -> (
    ClientOperationKey,
    Result<OperationIdentity, WriterError>,
    OperationInput,
    OperationAck,
) {
    match message {
        WriterMessage::Commit {
            key,
            config,
            base,
            ack,
        } => (
            key,
            commit_identity(&config, scope),
            OperationInput::Commit(*config, base),
            OperationAck::Commit(ack),
        ),
        WriterMessage::Mutate { key, mutation, ack } => {
            let identity = mutate_identity(&mutation, scope);
            (
                key,
                identity,
                OperationInput::Mutate(mutation),
                OperationAck::Mutate(ack),
            )
        }
        WriterMessage::Reload { key, ack } => (
            key,
            reload_identity(scope),
            OperationInput::Reload,
            OperationAck::Reload(ack),
        ),
        WriterMessage::BeginShutdown { .. } => {
            unreachable!("control messages are handled before unpack")
        }
        #[cfg(test)]
        WriterMessage::Lookup { .. } => {
            unreachable!("control messages are handled before unpack")
        }
    }
}
fn reply_error(ack: OperationAck, error: WriterError) {
    match ack {
        OperationAck::Commit(tx) => {
            let _ = tx.send(Err(error));
        }
        OperationAck::Mutate(tx) => {
            let _ = tx.send(Err(error));
        }
        OperationAck::Reload(tx) => {
            let _ = tx.send(Err(error));
        }
    }
}
fn reply_terminal_ack(ack: OperationAck, receipt: OperationReceipt, terminal: &TerminalResponse) {
    match (ack, terminal) {
        (OperationAck::Commit(tx), TerminalResponse::Commit { revision }) => {
            let _ = tx.send(Ok((receipt, *revision)));
        }
        (OperationAck::Mutate(tx), TerminalResponse::Mutate { revision, message }) => {
            let _ = tx.send(Ok((receipt, *revision, message.clone())));
        }
        (OperationAck::Reload(tx), TerminalResponse::Reload { config }) => {
            let _ = tx.send(Ok((receipt, (**config).clone())));
        }
        (ack, TerminalResponse::Error(error)) => reply_error(ack, error.clone()),
        (ack, _) => reply_error(ack, WriterError::OperationKeyConflict),
    }
}
fn prepare_commit(config: Config, revision: u64) -> Result<(Config, u64, Vec<u8>), WriterError> {
    config
        .validate()
        .map_err(|error| WriterError::CommitFailed(error.to_string()))?;
    let next = revision
        .checked_add(1)
        .ok_or(WriterError::RevisionExhausted)?;
    let bytes = crate::config_edit::canonical_format(&config)
        .map_err(|error| WriterError::CommitFailed(error.to_string()))?
        .into_bytes();
    Ok((config, next, bytes))
}
fn send_error(message: WriterMessage, error: WriterError) {
    match message {
        WriterMessage::Commit { ack, .. } => {
            let _ = ack.send(Err(error));
        }
        WriterMessage::Mutate { ack, .. } => {
            let _ = ack.send(Err(error));
        }
        WriterMessage::Reload { ack, .. } => {
            let _ = ack.send(Err(error));
        }
        #[cfg(test)]
        WriterMessage::Lookup { ack, .. } => {
            let _ = ack.send(Err(error));
        }
        WriterMessage::BeginShutdown { ack } => {
            let _ = ack.send(());
        }
    }
}
fn reject_unadmitted(message: WriterMessage) {
    send_error(message, WriterError::ServiceShuttingDown);
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeGeneration {
    revision: u64,
    config: Arc<Config>,
}
impl RuntimeGeneration {
    pub(crate) fn new(config: Config, revision: u64) -> Self {
        Self {
            revision,
            config: Arc::new(config),
        }
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn config(&self) -> &Config {
        &self.config
    }
}
#[derive(Clone)]
pub(crate) struct GenerationReader {
    cell: Arc<RwLock<Arc<RuntimeGeneration>>>,
}
impl GenerationReader {
    pub(crate) fn new(config: Config) -> Self {
        Self::from_generation(RuntimeGeneration::new(config, 0))
    }
    pub(crate) fn from_generation(generation: RuntimeGeneration) -> Self {
        Self {
            cell: Arc::new(RwLock::new(Arc::new(generation))),
        }
    }
    pub(crate) fn load(&self) -> Arc<RuntimeGeneration> {
        match self.cell.read() {
            Ok(g) => Arc::clone(&g),
            Err(p) => Arc::clone(p.get_ref()),
        }
    }
    pub(crate) fn publish(&self, config: Config, revision: u64) {
        let mut g = match self.cell.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g = Arc::new(RuntimeGeneration::new(config, revision));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str("[server]\nlisten = \"127.0.0.1:0\"\n").expect("config")
    }

    fn writer(
        initial: Config,
    ) -> (
        tempfile::TempDir,
        ConfigWriterHandle,
        GenerationReader,
        Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        tokio::task::JoinHandle<()>,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config_edit::write_full_config(&path, &initial).expect("initial config");
        let runtime = temp.path().join("runtime");
        let state_dirs = crate::service::StateDirs::for_test(&runtime);
        let core = Arc::new(tokio::sync::Mutex::new(
            crate::core::CoreState::from_config_with_state_dirs(
                initial.clone(),
                path.clone(),
                state_dirs,
            ),
        ));
        let generation = GenerationReader::new(initial);
        let (writer, task) = ConfigWriterHandle::start(
            99,
            8,
            generation.clone(),
            core.clone(),
            path,
            runtime,
            FatalConfigState::default(),
        );
        (temp, writer, generation, core, task)
    }

    fn add(id: &str) -> crate::model::ConfigMutation {
        crate::model::ConfigMutation::Add {
            model_id: id.to_string(),
            context_window: Some(1024),
            max_output_tokens: Some(128),
            copy_from: None,
            routes: None,
        }
    }

    #[tokio::test]
    async fn mutation_uses_the_latest_published_baseline_for_same_target() {
        let (_temp, writer, generation, _core, task) = writer(config());
        let first = writer.mutate(ClientOperationKey(1), add("same")).await;
        let second = writer.mutate(ClientOperationKey(2), add("same")).await;
        assert!(first.is_ok());
        assert!(
            matches!(second, Err(WriterError::MutationRejected(error)) if error.contains("already exists"))
        );
        assert_eq!(generation.load().revision(), 1);
        assert!(generation.load().config().models.contains_key("same"));
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn different_model_mutations_preserve_both_and_publish_contiguous_revisions() {
        let (_temp, writer, generation, core, task) = writer(config());
        let (_, first, _) = writer
            .mutate(ClientOperationKey(1), add("a"))
            .await
            .expect("a");
        let (_, second, _) = writer
            .mutate(ClientOperationKey(2), add("b"))
            .await
            .expect("b");
        assert_eq!((first, second), (1, 2));
        let snapshot = generation.load();
        assert!(snapshot.config().models.contains_key("a"));
        assert!(snapshot.config().models.contains_key("b"));
        let core = core.lock().await;
        assert!(core.config().models.contains_key("a"));
        assert!(core.config().models.contains_key("b"));
        drop(core);
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn commit_then_mutate_uses_committed_configuration_as_baseline() {
        let (_temp, writer, generation, _core, task) = writer(config());
        let mut committed = config();
        committed.server.as_mut().unwrap().listen = "127.0.0.1:19090".to_string();
        writer
            .commit(ClientOperationKey(1), committed, 0)
            .await
            .expect("commit");
        writer
            .mutate(ClientOperationKey(2), add("after-commit"))
            .await
            .expect("mutation");
        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 2);
        assert_eq!(
            snapshot.config().server.as_ref().unwrap().listen,
            "127.0.0.1:19090"
        );
        assert!(snapshot.config().models.contains_key("after-commit"));
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    // (D3 form B): a commit whose base revision is stale must be
    // rejected fail-stop instead of silently overwriting the newer generation.
    #[tokio::test]
    async fn commit_with_stale_base_is_rejected_and_first_commit_survives() {
        let (_temp, writer, generation, _core, task) = writer(config());
        let mut first_cfg = config();
        if let Some(ref mut s) = first_cfg.server {
            s.listen = "127.0.0.1:19099".to_string();
        }
        writer
            .commit(ClientOperationKey(1), first_cfg.clone(), 0)
            .await
            .expect("first commit");
        let mut stale_cfg = first_cfg.clone();
        if let Some(ref mut s) = stale_cfg.server {
            s.listen = "127.0.0.1:19098".to_string();
        }
        assert_eq!(
            writer.commit(ClientOperationKey(2), stale_cfg, 0).await,
            Err(WriterError::CommitBaseConflict {
                expected: 0,
                actual: 1
            }),
            "stale base must be rejected, not overwrite"
        );
        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 1, "rejected commit must not publish");
        assert_eq!(
            snapshot.config().server.as_ref().unwrap().listen,
            "127.0.0.1:19099",
            "first commit's content must survive"
        );
        let mut fresh_cfg = snapshot.config().clone();
        if let Some(ref mut s) = fresh_cfg.server {
            s.listen = "127.0.0.1:19097".to_string();
        }
        writer
            .commit(ClientOperationKey(3), fresh_cfg, snapshot.revision())
            .await
            .expect("retry with fresh base");
        assert_eq!(generation.load().revision(), 2);
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn reload_publishes_without_replacing_and_mutation_uses_loaded_baseline() {
        let (temp, writer, generation, _core, task) = writer(config());
        let path = temp.path().join("config.toml");
        let mut on_disk = config();
        if let Some(ref mut s) = on_disk.server {
            s.listen = "127.0.0.1:19091".to_string();
        }
        crate::config_edit::write_full_config(&path, &on_disk).expect("disk config");
        let before = std::fs::read(&path).expect("before reload");
        writer.reload(ClientOperationKey(1)).await.expect("reload");
        writer
            .mutate(ClientOperationKey(2), add("after-reload"))
            .await
            .expect("mutation");
        assert_eq!(
            generation.load().config().server.as_ref().unwrap().listen,
            "127.0.0.1:19091"
        );
        assert!(
            generation
                .load()
                .config()
                .models
                .contains_key("after-reload")
        );
        assert_ne!(
            std::fs::read(&path).expect("after mutation"),
            before,
            "mutation, not reload, performs the replacement"
        );
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn same_identity_replays_and_cross_kind_key_conflicts_without_extra_publish() {
        let (_temp, writer, generation, _core, task) = writer(config());
        let key = ClientOperationKey(7);
        let first = writer.mutate(key, add("replayed")).await.expect("first");
        let replay = writer.mutate(key, add("replayed")).await.expect("replay");
        assert_eq!(first, replay);
        assert_eq!(generation.load().revision(), 1);
        assert_eq!(
            writer.commit(key, config(), 1).await,
            Err(WriterError::OperationKeyConflict)
        );
        assert_eq!(generation.load().revision(), 1);
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn shutdown_closes_the_mailbox_and_join_proves_no_later_publish() {
        let (_temp, writer, generation, _core, task) = writer(config());
        writer.shutdown().await.expect("shutdown boundary");
        assert_eq!(
            writer.mutate(ClientOperationKey(9), add("late")).await,
            Err(WriterError::ServiceShuttingDown)
        );
        task.await.expect("join");
        assert_eq!(generation.load().revision(), 0);
    }

    #[tokio::test]
    async fn unknown_persistence_transitions_to_fail_stop_actor_exits_and_future_mutations_rejected()
     {
        let (temp, writer, _generation, _core, task) = writer(config());
        let path = temp.path().join("config.toml");
        // Replace config.toml with a directory so write_replacement's rename fails with EISDIR.
        // On rename failure, write_replacement returns ReplaceOutcome::Unknown.
        std::fs::remove_file(&path).expect("remove config file");
        std::fs::create_dir(&path).expect("create blocker directory");

        let key = ClientOperationKey(101);
        let res = writer.mutate(key, add("must-fail-unknown")).await;
        assert_eq!(res, Err(WriterError::RetrySafetyUnknown));

        // Verify gateway fatal waiting cannot miss the failure notification
        assert!(writer.fatal_config_state().is_failed());
        writer.fatal_config_state().wait().await;

        // Verify that the writer actor stopped receiving messages and its join handle completed
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("actor task must complete upon fail-stop")
            .expect("actor join must succeed");

        // Verify that future mutations and operations are rejected after fail-stop
        let next_mutate = writer
            .mutate(ClientOperationKey(102), add("future-model"))
            .await;
        assert_eq!(next_mutate, Err(WriterError::FailedStop));

        let next_commit = writer.commit(ClientOperationKey(103), config(), 0).await;
        assert_eq!(next_commit, Err(WriterError::FailedStop));

        let next_reload = writer.reload(ClientOperationKey(104)).await;
        assert!(matches!(next_reload, Err(WriterError::FailedStop)));

        // Verify shutdown returns FailedStop and does not hang
        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));
    }

    #[tokio::test]
    async fn fatal_config_state_wait_does_not_miss_notification() {
        let fatal = FatalConfigState::default();
        assert!(!fatal.is_failed());

        // Test 1: wait after fail returns immediately
        fatal.fail();
        assert!(fatal.is_failed());
        fatal.wait().await;

        // Test 2: wait before fail wakes up reliably without lost notification
        let fatal2 = FatalConfigState::default();
        let fatal2_clone = fatal2.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let wait_task = tokio::spawn(async move {
            let _ = started_tx.send(());
            fatal2_clone.wait().await;
        });
        started_rx.await.expect("wait task started");
        fatal2.fail();
        tokio::time::timeout(Duration::from_secs(2), wait_task)
            .await
            .expect("wait task must wake up without lost notification")
            .expect("wait task join");
        assert!(fatal2.is_failed());
    }

    fn writer_with_hook(
        initial: Config,
        hook: Option<ShutdownTestHook>,
    ) -> (
        tempfile::TempDir,
        ConfigWriterHandle,
        GenerationReader,
        Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        tokio::task::JoinHandle<()>,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config_edit::write_full_config(&path, &initial).expect("initial config");
        let runtime = temp.path().join("runtime");
        let state_dirs = crate::service::StateDirs::for_test(&runtime);
        let core = Arc::new(tokio::sync::Mutex::new(
            crate::core::CoreState::from_config_with_state_dirs(
                initial.clone(),
                path.clone(),
                state_dirs,
            ),
        ));
        let generation = GenerationReader::new(initial);
        let (writer, task) = ConfigWriterHandle::start_with_hook(
            99,
            8,
            generation.clone(),
            core.clone(),
            FixedScope::new(path, runtime),
            FatalConfigState::default(),
            hook,
        );
        (temp, writer, generation, core, task)
    }

    #[tokio::test]
    async fn shutdown_survives_dropped_initiating_caller_and_completes_for_later_caller() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let hook = ShutdownTestHook::new(started_tx, resume_rx);

        let (_temp, writer, generation, _core, task) = writer_with_hook(config(), Some(hook));

        // Initiating caller starts shutdown in a task
        let writer_for_caller1 = writer.clone();
        let mut caller1 = tokio::spawn(async move { writer_for_caller1.shutdown().await });

        // Deterministically wait for coordination to begin using the explicit channel
        started_rx.await.expect("shutdown coordination started");

        // Initiating caller is dropped/cancelled
        caller1.abort();
        let _ = (&mut caller1).await;

        // Release the coordination task to proceed
        resume_tx.send(()).expect("resume signal sent");

        // Later caller awaits shutdown and observes completion
        let second_result = writer.shutdown().await;
        assert_eq!(second_result, Ok(()));

        // Writer task must be completed (join completed, not abandoned)
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("writer task must join within timeout")
            .expect("writer task join succeeded");

        // Subsequent operations must be rejected with ServiceShuttingDown
        let late_op = writer
            .mutate(ClientOperationKey(888), add("late-model"))
            .await;
        assert_eq!(late_op, Err(WriterError::ServiceShuttingDown));
        assert_eq!(generation.load().revision(), 0);

        // Repeated shutdown call returns the same shared completion
        assert_eq!(writer.shutdown().await, Ok(()));
    }

    /// 「不可判定必须区别于成功」回归基线。
    ///
    /// 让关闭协调任务在产出结论之前被永久卡住（`pause_before_send`），并把等待预算调到
    /// 50ms：`shutdown()` 必须在预算内返回 `StopUndetermined`。修复前这里是**无超时的
    /// `wait_for`**，所以本用例外层再包一个 2s 超时——修复前该超时必然触发（失败），
    /// 修复后由内部预算先行给出三态结论（通过）。
    /// 断言同时锁死三态互不相同：不可判定既不得伪装成 `Ok(())`（干净关闭），也不得
    /// 冒充 `FailedStop`（未经证实的 fail-stop）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_reports_stop_undetermined_when_conclusion_never_arrives() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let hook = ShutdownTestHook::new(started_tx, resume_rx);
        let (_temp, writer, _generation, _core, task) = writer_with_hook(config(), Some(hook));
        writer.set_shutdown_result_budget_for_test(Duration::from_millis(50));

        let caller = tokio::spawn({
            let writer = writer.clone();
            async move { writer.shutdown().await }
        });
        started_rx
            .await
            .expect("关闭协调任务必须在产出结论之前被挂起");

        let observed = tokio::time::timeout(Duration::from_secs(2), caller)
            .await
            .expect("shutdown() 必须在预算内给出可判定的结论，不得无限挂起")
            .expect("关闭调用方任务本身不得 panic");
        assert_eq!(observed, Err(WriterError::StopUndetermined));

        // 不可判定不是粘性的假终态：放开被卡住的协调任务后，真实结论照常覆盖上来。
        resume_tx.send(()).expect("释放协调任务");
        assert_eq!(writer.shutdown().await, Ok(()));
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("writer 任务必须退出")
            .expect("writer 任务 join 成功");
    }

    #[tokio::test]
    async fn repeated_and_concurrent_shutdown_callers_await_one_shared_completion() {
        let (_temp, writer, _generation, _core, task) = writer(config());

        let w1 = writer.clone();
        let w2 = writer.clone();
        let w3 = writer.clone();

        let t1 = tokio::spawn(async move { w1.shutdown().await });
        let t2 = tokio::spawn(async move { w2.shutdown().await });
        let t3 = tokio::spawn(async move { w3.shutdown().await });

        let (r1, r2, r3) = tokio::join!(t1, t2, t3);
        assert_eq!(r1.expect("t1 join"), Ok(()));
        assert_eq!(r2.expect("t2 join"), Ok(()));
        assert_eq!(r3.expect("t3 join"), Ok(()));

        // Subsequent caller observes the stored completion
        assert_eq!(writer.shutdown().await, Ok(()));

        task.await.expect("writer join");
    }

    #[test]
    fn terminal_retention_caps_at_256_immediately_after_257_and_evicts_to_tombstone() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");

        // Construct 256 terminal operations using supplied Instant values
        for i in 1..=256 {
            let key = ClientOperationKey(i);
            assert!(matches!(
                retention.check(key, &identity),
                AdmissionCheck::Allow
            ));
            let receipt = OperationReceipt {
                key,
                id: OperationId {
                    boot_epoch: 1,
                    sequence: i as u64,
                },
            };
            retention.admit(receipt, identity.clone());
            retention.register_terminal(
                receipt.id,
                TerminalResponse::Commit { revision: i as u64 },
                base,
            );
            assert_eq!(retention.terminal_len(), i as usize);
            assert_eq!(retention.operations_len(), i as usize);
            assert_eq!(retention.tombstones_len(), 0);
        }

        assert_eq!(retention.terminal_len(), 256);
        assert_eq!(retention.operations_len(), 256);
        assert!(retention.contains_key(&ClientOperationKey(1)));

        // Construct terminal record number 257 using supplied Instant value
        let key_257 = ClientOperationKey(257);
        assert!(matches!(
            retention.check(key_257, &identity),
            AdmissionCheck::Allow
        ));
        let receipt_257 = OperationReceipt {
            key: key_257,
            id: OperationId {
                boot_epoch: 1,
                sequence: 257,
            },
        };
        retention.admit(receipt_257, identity);
        retention.register_terminal(
            receipt_257.id,
            TerminalResponse::Commit { revision: 257 },
            base,
        );

        // Immediately capped at 256 without waiting for next mailbox message
        assert_eq!(retention.terminal_len(), 256);
        assert_eq!(retention.operations_len(), 256);

        // Oldest key (key 1) was evicted and became a tombstone
        assert!(!retention.contains_key(&ClientOperationKey(1)));
        assert!(retention.contains_tombstone(&ClientOperationKey(1)));
        assert_eq!(retention.tombstones_len(), 1);

        // Most recent key (key 257) remains retained
        assert!(retention.contains_key(&ClientOperationKey(257)));
    }

    #[test]
    fn terminal_ttl_retained_strictly_before_600s_and_evicted_exactly_at_600s() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");
        let key = ClientOperationKey(100);
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 1,
                sequence: 1,
            },
        };

        retention.admit(receipt, identity.clone());
        retention.register_terminal(receipt.id, TerminalResponse::Commit { revision: 1 }, base);

        // Strictly before base + 600s: retained
        let before_600 = base + Duration::from_secs(599);
        retention.expire(before_600);
        assert_eq!(retention.terminal_len(), 1);
        assert!(retention.contains_key(&key));
        assert!(!retention.contains_tombstone(&key));
        assert!(matches!(
            retention.check(key, &identity),
            AdmissionCheck::Replay { .. }
        ));

        // Exactly at base + 600s: evicted to tombstone
        let at_600 = base + Duration::from_secs(600);
        retention.expire(at_600);
        assert_eq!(retention.terminal_len(), 0);
        assert!(!retention.contains_key(&key));
        assert!(retention.contains_tombstone(&key));
        assert_eq!(retention.tombstones_len(), 1);
    }

    #[test]
    fn successful_replay_does_not_extend_terminal_at_or_eviction_order() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");

        let key1 = ClientOperationKey(1);
        let receipt1 = OperationReceipt {
            key: key1,
            id: OperationId {
                boot_epoch: 1,
                sequence: 1,
            },
        };
        retention.admit(receipt1, identity.clone());
        retention.register_terminal(receipt1.id, TerminalResponse::Commit { revision: 1 }, base);

        let key2 = ClientOperationKey(2);
        let receipt2 = OperationReceipt {
            key: key2,
            id: OperationId {
                boot_epoch: 1,
                sequence: 2,
            },
        };
        let t_op2 = base + Duration::from_secs(10);
        retention.admit(receipt2, identity.clone());
        retention.register_terminal(receipt2.id, TerminalResponse::Commit { revision: 2 }, t_op2);

        assert_eq!(retention.terminal_order(), vec![receipt1.id, receipt2.id]);
        assert_eq!(retention.get_terminal_at(&receipt1.id), Some(base));

        // Replay key1 at base + 300s
        let replay_check = retention.check(key1, &identity);
        assert!(matches!(replay_check, AdmissionCheck::Replay { .. }));

        // Assert terminal_at was NOT extended by replay
        assert_eq!(retention.get_terminal_at(&receipt1.id), Some(base));
        // Assert eviction order was NOT changed or moved to back
        assert_eq!(retention.terminal_order(), vec![receipt1.id, receipt2.id]);

        // At base + 600s (only 300s after replay, but 600s after initial completion)
        retention.expire(base + Duration::from_secs(600));

        // key1 must be evicted at its original TTL, while key2 remains retained until base + 610s
        assert_eq!(retention.terminal_order(), vec![receipt2.id]);
        assert!(!retention.contains_key(&key1));
        assert!(retention.contains_tombstone(&key1));
        assert!(retention.contains_key(&key2));
    }

    #[test]
    fn evicted_key_rejected_as_retry_safety_unknown_while_tombstone_live() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");
        let key = ClientOperationKey(1);
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 1,
                sequence: 1,
            },
        };

        retention.admit(receipt, identity.clone());
        retention.register_terminal(receipt.id, TerminalResponse::Commit { revision: 1 }, base);

        // Evict to tombstone at base + 600s
        let t_evict = base + Duration::from_secs(600);
        retention.expire(t_evict);
        assert!(retention.contains_tombstone(&key));

        // While tombstone is live (e.g. at t_evict + 300s), retry must be rejected
        let check_during_live = retention.check(key, &identity);
        assert!(matches!(
            check_during_live,
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));
        assert_eq!(retention.operations_len(), 0);

        // Even at t_evict + 599s (strictly before tombstone TTL of 600s)
        retention.expire(t_evict + Duration::from_secs(599));
        assert!(retention.contains_tombstone(&key));
        assert!(matches!(
            retention.check(key, &identity),
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));
    }

    #[test]
    fn tombstone_expiry_at_ttl_allows_same_key_admitted_as_new_operation() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");
        let key = ClientOperationKey(1);
        let receipt1 = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 1,
                sequence: 1,
            },
        };

        retention.admit(receipt1, identity.clone());
        retention.register_terminal(receipt1.id, TerminalResponse::Commit { revision: 1 }, base);

        // Evict to tombstone at base + 600s
        let t_evict = base + Duration::from_secs(600);
        retention.expire(t_evict);
        assert!(retention.contains_tombstone(&key));

        // Exactly at tombstone TTL: t_evict + 600s
        let t_tombstone_expire = t_evict + Duration::from_secs(600);
        retention.expire(t_tombstone_expire);
        assert!(!retention.contains_tombstone(&key));

        // The same key is now allowed as a brand new operation
        assert!(matches!(
            retention.check(key, &identity),
            AdmissionCheck::Allow
        ));

        // Admit as a new operation with new sequence
        let receipt2 = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 1,
                sequence: 2,
            },
        };
        retention.admit(receipt2, identity);
        retention.register_terminal(
            receipt2.id,
            TerminalResponse::Commit { revision: 2 },
            t_tombstone_expire,
        );

        assert_eq!(retention.terminal_len(), 1);
        assert!(retention.contains_key(&key));
        assert!(!retention.contains_tombstone(&key));
    }

    #[test]
    fn receipt_lookup_distinguishes_retained_tombstoned_unknown_and_wrong_epoch() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let boot_epoch = 42_u128;
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"test-tag");

        let key1 = ClientOperationKey(1);
        let receipt1 = OperationReceipt {
            key: key1,
            id: OperationId {
                boot_epoch,
                sequence: 1,
            },
        };

        retention.admit(receipt1, identity);
        retention.register_terminal(receipt1.id, TerminalResponse::Commit { revision: 1 }, base);

        // 1. Retained terminal receipt returns stored terminal result
        let res_retained = retention.lookup(receipt1, boot_epoch);
        assert_eq!(res_retained, Ok(TerminalResponse::Commit { revision: 1 }));

        // 2. Wrong boot_epoch is unknown
        let wrong_epoch_receipt = OperationReceipt {
            key: key1,
            id: OperationId {
                boot_epoch: boot_epoch + 1,
                sequence: 1,
            },
        };
        assert_eq!(
            retention.lookup(wrong_epoch_receipt, boot_epoch),
            Err(WriterError::UnknownOperation)
        );

        // 3. Unrelated / unknown receipt is unknown
        let unrelated = OperationReceipt {
            key: ClientOperationKey(999),
            id: OperationId {
                boot_epoch,
                sequence: 999,
            },
        };
        assert_eq!(
            retention.lookup(unrelated, boot_epoch),
            Err(WriterError::UnknownOperation)
        );

        // Sequence matches but wrong key is unknown
        let mismatched_key = OperationReceipt {
            key: ClientOperationKey(2),
            id: OperationId {
                boot_epoch,
                sequence: 1,
            },
        };
        assert_eq!(
            retention.lookup(mismatched_key, boot_epoch),
            Err(WriterError::UnknownOperation)
        );

        // 4. Evicted and tombstoned receipt returns OperationExpired
        let t_evict = base + Duration::from_secs(600);
        retention.expire(t_evict);
        assert!(!retention.contains_key(&key1));
        assert!(retention.contains_tombstone(&key1));
        assert_eq!(
            retention.lookup(receipt1, boot_epoch),
            Err(WriterError::OperationExpired)
        );

        // 5. When tombstone expires at its TTL, it becomes UnknownOperation
        let t_tombstone_expired = t_evict + Duration::from_secs(600);
        retention.expire(t_tombstone_expired);
        assert!(!retention.contains_tombstone(&key1));
        assert_eq!(
            retention.lookup(receipt1, boot_epoch),
            Err(WriterError::UnknownOperation)
        );
    }

    #[test]
    fn tombstone_capacity_caps_at_256_retains_newest_and_cleans_up_oldest() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let boot_epoch = 1_u128;
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");

        // 1. Admit and complete 256 operations (keys 1..=256).
        // At this point, 256 operations are terminal; 0 tombstones exist.
        for i in 1..=256 {
            let key = ClientOperationKey(i);
            let receipt = OperationReceipt {
                key,
                id: OperationId {
                    boot_epoch,
                    sequence: i as u64,
                },
            };
            retention.admit(receipt, identity.clone());
            retention.register_terminal(
                receipt.id,
                TerminalResponse::Commit { revision: i as u64 },
                base + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(retention.terminal_len(), 256);
        assert_eq!(retention.tombstones_len(), 0);

        // 2. Admit and complete operations 257..=512 (256 additional operations).
        // Each causes one terminal eviction into tombstones.
        // After 512 operations in total, keys 1..=256 have been evicted into tombstones (256 tombstones).
        for i in 257..=512 {
            let key = ClientOperationKey(i);
            let receipt = OperationReceipt {
                key,
                id: OperationId {
                    boot_epoch,
                    sequence: i as u64,
                },
            };
            retention.admit(receipt, identity.clone());
            retention.register_terminal(
                receipt.id,
                TerminalResponse::Commit { revision: i as u64 },
                base + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(retention.terminal_len(), 256);
        assert_eq!(retention.tombstones_len(), 256);
        assert_eq!(retention.tombstones_by_id.len(), 256);

        // Key 1 is currently the oldest tombstone.
        let receipt_1 = OperationReceipt {
            key: ClientOperationKey(1),
            id: OperationId {
                boot_epoch,
                sequence: 1,
            },
        };
        assert!(retention.contains_tombstone(&receipt_1.key));
        assert_eq!(
            retention.lookup(receipt_1, boot_epoch),
            Err(WriterError::OperationExpired)
        );
        assert!(matches!(
            retention.check(receipt_1.key, &identity),
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));

        // 3. Admit and complete operation 513.
        // Terminal capacity eviction evicts key 257 into tombstones, creating the 257th tombstone.
        // Tombstone capacity eviction immediately triggers, capping tombstones at 256 and popping the oldest tombstone (key 1).
        let receipt_513 = OperationReceipt {
            key: ClientOperationKey(513),
            id: OperationId {
                boot_epoch,
                sequence: 513,
            },
        };
        retention.admit(receipt_513, identity.clone());
        retention.register_terminal(
            receipt_513.id,
            TerminalResponse::Commit { revision: 513 },
            base + Duration::from_secs(513),
        );

        // Retain newest 256 only
        assert_eq!(retention.tombstones_len(), 256);
        assert_eq!(retention.tombstones_by_id.len(), 256);
        assert_eq!(retention.tombstone_order.len(), 256);

        // Oldest key (key 1) became admissible
        assert!(!retention.contains_tombstone(&receipt_1.key));
        assert!(matches!(
            retention.check(receipt_1.key, &identity),
            AdmissionCheck::Allow
        ));

        // Old receipt lookup for key 1 now returns UnknownOperation
        assert_eq!(
            retention.lookup(receipt_1, boot_epoch),
            Err(WriterError::UnknownOperation)
        );

        // Validate tombstones_by_id cleanup
        assert!(!retention.tombstones_by_id.contains_key(&receipt_1.id));

        // Validate insertion order: newest 256 tombstones are keys 2..=257 in exact FIFO order
        let order_keys: Vec<ClientOperationKey> =
            retention.tombstone_order.iter().map(|t| t.key).collect();
        let expected_keys: Vec<ClientOperationKey> = (2..=257).map(ClientOperationKey).collect();
        assert_eq!(order_keys, expected_keys);

        let order_ids: Vec<OperationId> = retention.tombstone_order.iter().map(|t| t.id).collect();
        let expected_ids: Vec<OperationId> = (2..=257)
            .map(|i| OperationId {
                boot_epoch,
                sequence: i as u64,
            })
            .collect();
        assert_eq!(order_ids, expected_ids);

        // All 256 retained tombstones are present in tombstones_by_id and return OperationExpired
        for k in 2..=257 {
            let id = OperationId {
                boot_epoch,
                sequence: k as u64,
            };
            assert_eq!(
                retention.tombstones_by_id.get(&id),
                Some(&ClientOperationKey(k))
            );
            assert!(retention.contains_tombstone(&ClientOperationKey(k)));
        }

        let receipt_2 = OperationReceipt {
            key: ClientOperationKey(2),
            id: OperationId {
                boot_epoch,
                sequence: 2,
            },
        };
        assert_eq!(
            retention.lookup(receipt_2, boot_epoch),
            Err(WriterError::OperationExpired)
        );
    }

    #[test]
    fn admitted_nonterminal_operation_survives_terminal_capacity_eviction_and_ttl_cleanup() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let boot_epoch = 1_u128;
        let identity = OperationIdentity::test_identity(OperationKind::Commit, b"tag");

        // 1. Admit an in-flight (nonterminal) operation
        let in_flight_key = ClientOperationKey(9999);
        let in_flight_receipt = OperationReceipt {
            key: in_flight_key,
            id: OperationId {
                boot_epoch,
                sequence: 9999,
            },
        };
        retention.admit(in_flight_receipt, identity.clone());

        // In-flight operation is active, but not in terminal_order
        assert_eq!(retention.operations_len(), 1);
        assert_eq!(retention.terminal_len(), 0);
        assert!(retention.contains_key(&in_flight_key));
        assert!(matches!(
            retention.check(in_flight_key, &identity),
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));
        assert_eq!(
            retention.lookup(in_flight_receipt, boot_epoch),
            Err(WriterError::RetrySafetyUnknown)
        );

        // 2. Admit and complete 257 terminal operations to trigger terminal 256/257 capacity eviction
        for i in 1..=257 {
            let key = ClientOperationKey(i);
            let receipt = OperationReceipt {
                key,
                id: OperationId {
                    boot_epoch,
                    sequence: i as u64,
                },
            };
            retention.admit(receipt, identity.clone());
            retention.register_terminal(
                receipt.id,
                TerminalResponse::Commit { revision: i as u64 },
                base + Duration::from_secs(i as u64),
            );
        }

        // Terminal capacity is capped at 256, and older terminal record (key 1) was evicted to tombstone
        assert_eq!(retention.terminal_len(), 256);
        assert_eq!(retention.tombstones_len(), 1);
        assert!(retention.contains_tombstone(&ClientOperationKey(1)));
        assert!(!retention.contains_key(&ClientOperationKey(1)));

        // In-flight operation survived terminal 256/257 capacity eviction
        assert_eq!(retention.operations_len(), 257); // 256 terminal + 1 in-flight
        assert!(retention.contains_key(&in_flight_key));
        assert!(!retention.contains_tombstone(&in_flight_key));
        assert!(matches!(
            retention.check(in_flight_key, &identity),
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));
        assert_eq!(
            retention.lookup(in_flight_receipt, boot_epoch),
            Err(WriterError::RetrySafetyUnknown)
        );

        // 3. Trigger TTL cleanup beyond 600s: all terminal operations expire
        let t_ttl = base + Duration::from_secs(1000);
        retention.expire(t_ttl);

        // All terminal records have been evicted from terminal_order
        assert_eq!(retention.terminal_len(), 0);

        // In-flight operation survived TTL cleanup
        assert_eq!(retention.operations_len(), 1);
        assert!(retention.contains_key(&in_flight_key));
        assert!(!retention.contains_tombstone(&in_flight_key));
        assert!(matches!(
            retention.check(in_flight_key, &identity),
            AdmissionCheck::Reject(WriterError::RetrySafetyUnknown)
        ));
        assert_eq!(
            retention.lookup(in_flight_receipt, boot_epoch),
            Err(WriterError::RetrySafetyUnknown)
        );

        // 4. Finally, complete the in-flight operation and verify replay and lookup
        let completion_time = t_ttl + Duration::from_secs(5);
        let terminal_resp = TerminalResponse::Commit { revision: 9999 };
        retention.register_terminal(in_flight_receipt.id, terminal_resp.clone(), completion_time);

        assert_eq!(retention.terminal_len(), 1);
        assert_eq!(retention.operations_len(), 1);
        let replay_check = retention.check(in_flight_key, &identity);
        assert!(matches!(
            replay_check,
            AdmissionCheck::Replay { receipt, terminal }
                if receipt == in_flight_receipt && terminal == terminal_resp
        ));
        assert_eq!(
            retention.lookup(in_flight_receipt, boot_epoch),
            Ok(terminal_resp)
        );
    }

    #[test]
    fn same_key_same_kind_conflicts_reject_without_changing_record_and_replay_succeeds() {
        let mut retention = RetentionStore::new();
        let base = Instant::now();
        let boot_epoch = 1_u128;
        let key = ClientOperationKey(42);

        // 32 prefix bytes shared between identical-digest test tags
        let prefix = b"01234567890123456789012345678901";

        // Original identity
        let mut orig_tag = prefix.to_vec();
        orig_tag.extend_from_slice(b"-canonical-original-payload");
        let original_identity = OperationIdentity::test_identity(OperationKind::Mutate, &orig_tag);

        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch,
                sequence: 42,
            },
        };
        retention.admit(receipt, original_identity.clone());
        let original_terminal = TerminalResponse::Mutate {
            revision: 10,
            message: "mutated successfully".to_string(),
        };
        retention.register_terminal(receipt.id, original_terminal.clone(), base);

        // 1. Conflict case A: different canonical payloads (different digest and canonical)
        let diff_payload_identity = OperationIdentity::test_identity(
            OperationKind::Mutate,
            b"completely-different-payload-and-digest",
        );
        assert_eq!(original_identity.kind, diff_payload_identity.kind);
        assert_ne!(original_identity.digest, diff_payload_identity.digest);
        assert_ne!(original_identity.canonical, diff_payload_identity.canonical);

        let conflict_a = retention.check(key, &diff_payload_identity);
        assert!(matches!(
            conflict_a,
            AdmissionCheck::Reject(WriterError::OperationKeyConflict)
        ));

        // Verify original receipt/record is unchanged
        assert_eq!(
            retention.lookup(receipt, boot_epoch),
            Ok(original_terminal.clone())
        );
        let stored_op = retention
            .operations
            .get(&receipt.id)
            .expect("stored operation exists");
        assert_eq!(stored_op.receipt, receipt);
        assert_eq!(stored_op.identity.kind, original_identity.kind);
        assert_eq!(stored_op.identity.digest, original_identity.digest);
        assert_eq!(stored_op.identity.canonical, original_identity.canonical);
        assert_eq!(stored_op.terminal, Some(original_terminal.clone()));

        // 2. Conflict case B: digest-equal-but-canonical-different identities
        let mut diff_canon_tag = prefix.to_vec();
        diff_canon_tag.extend_from_slice(b"-canonical-different-payload");
        let digest_equal_diff_canonical =
            OperationIdentity::test_identity(OperationKind::Mutate, &diff_canon_tag);

        assert_eq!(original_identity.kind, digest_equal_diff_canonical.kind);
        assert_eq!(original_identity.digest, digest_equal_diff_canonical.digest);
        assert_ne!(
            original_identity.canonical,
            digest_equal_diff_canonical.canonical
        );

        let conflict_b = retention.check(key, &digest_equal_diff_canonical);
        assert!(matches!(
            conflict_b,
            AdmissionCheck::Reject(WriterError::OperationKeyConflict)
        ));

        // Verify original receipt/record remains completely unchanged
        assert_eq!(
            retention.lookup(receipt, boot_epoch),
            Ok(original_terminal.clone())
        );
        let stored_op_after_b = retention
            .operations
            .get(&receipt.id)
            .expect("stored operation exists");
        assert_eq!(stored_op_after_b.receipt, receipt);
        assert_eq!(stored_op_after_b.identity.kind, original_identity.kind);
        assert_eq!(stored_op_after_b.identity.digest, original_identity.digest);
        assert_eq!(
            stored_op_after_b.identity.canonical,
            original_identity.canonical
        );
        assert_eq!(stored_op_after_b.terminal, Some(original_terminal.clone()));

        // 3. Original request still replays exactly
        let replay_check = retention.check(key, &original_identity);
        assert!(matches!(
            replay_check,
            AdmissionCheck::Replay {
                receipt: r,
                terminal: t,
            } if r == receipt && t == original_terminal
        ));
        assert_eq!(retention.lookup(receipt, boot_epoch), Ok(original_terminal));
    }

    #[test]
    fn canonical_identity_encoding_field_conflicts_and_replays_table() {
        use crate::config::Protocol;
        use crate::model::ConfigMutation;

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let state_dir = temp.path().join("runtime");
        let base_scope = FixedScope::new(config_path.clone(), state_dir.clone());
        let diff_path_scope =
            FixedScope::new(temp.path().join("other_config.toml"), state_dir.clone());
        let diff_state_scope =
            FixedScope::new(config_path.clone(), temp.path().join("other_runtime"));

        let mutate_id = |m: &ConfigMutation, s: &FixedScope| -> OperationIdentity {
            mutate_identity(m, s).expect("mutate identity")
        };
        let commit_id = |c: &Config, s: &FixedScope| -> OperationIdentity {
            commit_identity(c, s).expect("commit identity")
        };
        let reload_id =
            |s: &FixedScope| -> OperationIdentity { reload_identity(s).expect("reload identity") };

        let base_add = ConfigMutation::Add {
            model_id: "m".into(),
            context_window: Some(1000),
            max_output_tokens: Some(200),
            copy_from: None,
            routes: None,
        };
        let base_add_id = mutate_id(&base_add, &base_scope);
        let base_add_terminal = TerminalResponse::Mutate {
            revision: 1,
            message: "add ok".into(),
        };

        let base_remove = ConfigMutation::Remove {
            model_id: "m".into(),
            force: false,
        };
        let base_remove_id = mutate_id(&base_remove, &base_scope);
        let base_remove_terminal = TerminalResponse::Mutate {
            revision: 2,
            message: "remove ok".into(),
        };

        let base_move = ConfigMutation::ProviderMove {
            model_id: "m".into(),
            protocol: Protocol::OpenaiChatCompletions,
            provider_id: "p1".into(),
            to_index: 0,
        };
        let base_move_id = mutate_id(&base_move, &base_scope);
        let base_move_terminal = TerminalResponse::Mutate {
            revision: 3,
            message: "move ok".into(),
        };

        let cfg_bindings_base: Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[models.m]\ncontext_window = 1000\nmax_output_tokens = 100\n\n[[models.m.openai_chat_providers]]\nname = \"p1\"\nmodel = \"u1\"\n\n[[models.m.openai_chat_providers]]\nname = \"p2\"\nmodel = \"u2\"\n",
        ).expect("cfg_bindings_base");
        let cfg_bindings_rev: Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[models.m]\ncontext_window = 1000\nmax_output_tokens = 100\n\n[[models.m.openai_chat_providers]]\nname = \"p2\"\nmodel = \"u2\"\n\n[[models.m.openai_chat_providers]]\nname = \"p1\"\nmodel = \"u1\"\n",
        ).expect("cfg_bindings_rev");
        let base_commit_bindings_id = commit_id(&cfg_bindings_base, &base_scope);
        let base_commit_terminal = TerminalResponse::Commit { revision: 10 };

        let base_reload_id = reload_id(&base_scope);
        let base_reload_terminal = TerminalResponse::Reload {
            config: Box::new(cfg_bindings_base.clone()),
        };

        struct TestCase {
            name: &'static str,
            base_identity: OperationIdentity,
            base_terminal: TerminalResponse,
            conflicting_identity: OperationIdentity,
        }

        let cases = vec![
            TestCase {
                name: "mutation_changed_context",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::Add {
                        model_id: "m".into(),
                        context_window: Some(2000),
                        max_output_tokens: Some(200),
                        copy_from: None,
                        routes: None,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "mutation_changed_max_output",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::Add {
                        model_id: "m".into(),
                        context_window: Some(1000),
                        max_output_tokens: Some(400),
                        copy_from: None,
                        routes: None,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "mutation_changed_source",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::Add {
                        model_id: "m".into(),
                        context_window: Some(1000),
                        max_output_tokens: Some(200),
                        copy_from: Some("base-model".into()),
                        routes: None,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "remove_changed_force",
                base_identity: base_remove_id.clone(),
                base_terminal: base_remove_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::Remove {
                        model_id: "m".into(),
                        force: true,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "provider_move_changed_index",
                base_identity: base_move_id.clone(),
                base_terminal: base_move_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::ProviderMove {
                        model_id: "m".into(),
                        protocol: Protocol::OpenaiChatCompletions,
                        provider_id: "p1".into(),
                        to_index: 1,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "provider_move_changed_protocol",
                base_identity: base_move_id.clone(),
                base_terminal: base_move_terminal.clone(),
                conflicting_identity: mutate_id(
                    &ConfigMutation::ProviderMove {
                        model_id: "m".into(),
                        protocol: Protocol::Anthropic,
                        provider_id: "p1".into(),
                        to_index: 0,
                    },
                    &base_scope,
                ),
            },
            TestCase {
                name: "commit_ordered_bindings_reversed",
                base_identity: base_commit_bindings_id.clone(),
                base_terminal: base_commit_terminal.clone(),
                conflicting_identity: commit_id(&cfg_bindings_rev, &base_scope),
            },
            TestCase {
                name: "changed_config_path",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: mutate_id(&base_add, &diff_path_scope),
            },
            TestCase {
                name: "changed_state_scope",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: mutate_id(&base_add, &diff_state_scope),
            },
            TestCase {
                name: "cross_kind_mutate_to_commit",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: commit_id(&cfg_bindings_base, &base_scope),
            },
            TestCase {
                name: "cross_kind_mutate_to_reload",
                base_identity: base_add_id.clone(),
                base_terminal: base_add_terminal.clone(),
                conflicting_identity: reload_id(&base_scope),
            },
            TestCase {
                name: "cross_kind_commit_to_mutate",
                base_identity: base_commit_bindings_id.clone(),
                base_terminal: base_commit_terminal.clone(),
                conflicting_identity: base_add_id.clone(),
            },
            TestCase {
                name: "cross_kind_commit_to_reload",
                base_identity: base_commit_bindings_id.clone(),
                base_terminal: base_commit_terminal.clone(),
                conflicting_identity: reload_id(&base_scope),
            },
            TestCase {
                name: "cross_kind_reload_to_mutate",
                base_identity: base_reload_id.clone(),
                base_terminal: base_reload_terminal.clone(),
                conflicting_identity: base_add_id.clone(),
            },
            TestCase {
                name: "cross_kind_reload_to_commit",
                base_identity: base_reload_id.clone(),
                base_terminal: base_reload_terminal.clone(),
                conflicting_identity: commit_id(&cfg_bindings_base, &base_scope),
            },
        ];

        let mut retention = RetentionStore::new();
        let base_time = Instant::now();
        let boot_epoch = 1_u128;

        for (idx, case) in cases.iter().enumerate() {
            let key = ClientOperationKey(idx as u128 + 1);
            let receipt = OperationReceipt {
                key,
                id: OperationId {
                    boot_epoch,
                    sequence: idx as u64 + 1,
                },
            };

            assert_ne!(
                case.base_identity.canonical, case.conflicting_identity.canonical,
                "case '{}' canonical payloads must differ",
                case.name
            );

            retention.admit(receipt, case.base_identity.clone());
            retention.register_terminal(receipt.id, case.base_terminal.clone(), base_time);

            let conflict = retention.check(key, &case.conflicting_identity);
            assert!(
                matches!(
                    conflict,
                    AdmissionCheck::Reject(WriterError::OperationKeyConflict)
                ),
                "case '{}' expected OperationKeyConflict, got {:?}",
                case.name,
                conflict
            );

            assert_eq!(
                retention.lookup(receipt, boot_epoch),
                Ok(case.base_terminal.clone()),
                "case '{}' lookup altered",
                case.name
            );
            let stored = retention
                .operations
                .get(&receipt.id)
                .expect("stored record");
            assert_eq!(stored.receipt, receipt);
            assert_eq!(stored.identity.kind, case.base_identity.kind);
            assert_eq!(stored.identity.digest, case.base_identity.digest);
            assert_eq!(stored.identity.canonical, case.base_identity.canonical);
            assert_eq!(stored.terminal, Some(case.base_terminal.clone()));

            let replay = retention.check(key, &case.base_identity);
            assert!(
                matches!(
                    replay,
                    AdmissionCheck::Replay {
                        receipt: r,
                        terminal: ref t,
                    } if r == receipt && t == &case.base_terminal
                ),
                "case '{}' expected replay, got {:?}",
                case.name,
                replay
            );
            assert_eq!(
                retention.lookup(receipt, boot_epoch),
                Ok(case.base_terminal.clone())
            );
        }

        let cfg_map_a: Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[models.m1]\ncontext_window = 1000\nmax_output_tokens = 100\n\n[models.m2]\ncontext_window = 2000\nmax_output_tokens = 200\n",
        ).expect("cfg_map_a");
        let cfg_map_b: Config = toml::from_str(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[models.m2]\ncontext_window = 2000\nmax_output_tokens = 200\n\n[models.m1]\ncontext_window = 1000\nmax_output_tokens = 100\n",
        ).expect("cfg_map_b");

        let id_map_a = commit_id(&cfg_map_a, &base_scope);
        let id_map_b = commit_id(&cfg_map_b, &base_scope);
        assert_eq!(id_map_a.digest, id_map_b.digest);
        assert_eq!(id_map_a.canonical, id_map_b.canonical);

        let key_map = ClientOperationKey(999);
        let receipt_map = OperationReceipt {
            key: key_map,
            id: OperationId {
                boot_epoch,
                sequence: 999,
            },
        };
        let term_map = TerminalResponse::Commit { revision: 99 };
        retention.admit(receipt_map, id_map_a);
        retention.register_terminal(receipt_map.id, term_map.clone(), base_time);

        let replay_map = retention.check(key_map, &id_map_b);
        assert!(matches!(
            replay_map,
            AdmissionCheck::Replay { receipt, terminal }
                if receipt == receipt_map && terminal == term_map
        ));
    }

    #[tokio::test]
    async fn writer_receipt_lookup_routes_through_mailbox_and_respects_lifecycle() {
        let (_temp, writer, _generation, _core, task) = writer(config());

        let key = ClientOperationKey(55);
        let (receipt, rev, _msg) = writer
            .mutate(key, add("lookup-test-model"))
            .await
            .expect("mutate succeeds");

        // Lookup retained terminal receipt through the mailbox
        let looked_up = writer.lookup(receipt).await;
        assert!(matches!(
            looked_up,
            Ok(TerminalResponse::Mutate { revision, .. }) if revision == rev
        ));

        // Wrong boot_epoch
        let wrong_epoch_receipt = OperationReceipt {
            key: receipt.key,
            id: OperationId {
                boot_epoch: receipt.id.boot_epoch + 1,
                sequence: receipt.id.sequence,
            },
        };
        assert_eq!(
            writer.lookup(wrong_epoch_receipt).await,
            Err(WriterError::UnknownOperation)
        );

        // Unknown receipt
        let unknown = OperationReceipt {
            key: ClientOperationKey(9999),
            id: OperationId {
                boot_epoch: receipt.id.boot_epoch,
                sequence: 9999,
            },
        };
        assert_eq!(
            writer.lookup(unknown).await,
            Err(WriterError::UnknownOperation)
        );

        // Shutdown the writer
        writer.shutdown().await.expect("shutdown succeeds");
        task.await.expect("task join");

        // After shutdown, lookup is rejected according to existing lifecycle
        let after_shutdown = writer.lookup(receipt).await;
        assert_eq!(after_shutdown, Err(WriterError::ServiceShuttingDown));
    }

    fn writer_with_all_controls(
        initial: Config,
        test_hook: Option<ShutdownTestHook>,
        phase_hook: Option<PhaseHook>,
        persistence: Option<TestPersistence>,
    ) -> (
        tempfile::TempDir,
        ConfigWriterHandle,
        GenerationReader,
        Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        tokio::task::JoinHandle<()>,
        PathBuf,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.toml");
        crate::config_edit::write_full_config(&path, &initial).expect("initial config");
        let runtime = temp.path().join("runtime");
        let state_dirs = crate::service::StateDirs::for_test(&runtime);
        let core = Arc::new(tokio::sync::Mutex::new(
            crate::core::CoreState::from_config_with_state_dirs(
                initial.clone(),
                path.clone(),
                state_dirs,
            ),
        ));
        let generation = GenerationReader::new(initial);
        let mut controls = WriterTestControls::with_phase_and_persistence(phase_hook, persistence);
        controls.test_hook = test_hook;
        let (writer, task) = ConfigWriterHandle::start_with_controls(
            99,
            8,
            generation.clone(),
            core.clone(),
            FixedScope::new(path.clone(), runtime),
            FatalConfigState::default(),
            controls,
        );
        (temp, writer, generation, core, task, path)
    }

    fn writer_with_controls(
        initial: Config,
        phase_hook: Option<PhaseHook>,
        persistence: Option<TestPersistence>,
    ) -> (
        tempfile::TempDir,
        ConfigWriterHandle,
        GenerationReader,
        Arc<tokio::sync::Mutex<crate::core::CoreState>>,
        tokio::task::JoinHandle<()>,
        PathBuf,
    ) {
        writer_with_all_controls(initial, None, phase_hook, persistence)
    }

    #[tokio::test]
    async fn phase_control_same_target_mutations_rejects_second() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, _core, task, _path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let mut pause_a = phase_hook.pause(1, OperationPhase::Prepared);
        let writer_a = writer.clone();
        let task_a = tokio::spawn(async move {
            writer_a
                .mutate(ClientOperationKey(1), add("same_target"))
                .await
        });

        pause_a.wait_reached().await;

        let writer_b = writer.clone();
        let task_b = tokio::spawn(async move {
            writer_b
                .mutate(ClientOperationKey(2), add("same_target"))
                .await
        });

        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // While A is held at Prepared:
        // B has entered the mailbox, but cannot be admitted yet because the actor processes sequentially
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert!(!task_a.is_finished());
        assert!(!task_b.is_finished());

        pause_a.release();

        let res_a = task_a.await.unwrap();
        let res_b = task_b.await.unwrap();

        assert!(res_a.is_ok());
        assert!(
            matches!(res_b, Err(WriterError::MutationRejected(err)) if err.contains("already exists"))
        );

        // Exactly one replacement and one publication
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 1);
        assert!(
            generation
                .load()
                .config()
                .models
                .contains_key("same_target")
        );
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Published), 0);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_different_target_mutations_preserves_both_and_contiguous_revisions() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let mut pause_a = phase_hook.pause(1, OperationPhase::Prepared);
        let writer_a = writer.clone();
        let task_a =
            tokio::spawn(
                async move { writer_a.mutate(ClientOperationKey(1), add("model_a")).await },
            );

        pause_a.wait_reached().await;

        let writer_b = writer.clone();
        let task_b =
            tokio::spawn(
                async move { writer_b.mutate(ClientOperationKey(2), add("model_b")).await },
            );

        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // While A is held at Prepared:
        // B has entered the mailbox, but cannot be admitted yet because the actor processes sequentially
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert!(!task_a.is_finished());
        assert!(!task_b.is_finished());

        pause_a.release();

        let (_, rev_a, _) = task_a.await.unwrap().expect("a succeeds");
        let (_, rev_b, _) = task_b.await.unwrap().expect("b succeeds");

        assert_eq!((rev_a, rev_b), (1, 2));
        assert_eq!(persistence.call_count(), 2);

        let disk_cfg = Config::load(&path).expect("load disk config");
        assert!(disk_cfg.models.contains_key("model_a"));
        assert!(disk_cfg.models.contains_key("model_b"));

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 2);
        assert!(snapshot.config().models.contains_key("model_a"));
        assert!(snapshot.config().models.contains_key("model_b"));

        let core_guard = core.lock().await;
        assert!(core_guard.config().models.contains_key("model_a"));
        assert!(core_guard.config().models.contains_key("model_b"));
        drop(core_guard);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_commit_then_mutate_uses_committed_baseline() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, _path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let mut committed_cfg = config();
        committed_cfg.models.insert(
            "committed_model".to_string(),
            toml::from_str("context_window = 2048\nmax_output_tokens = 256\n")
                .expect("model config"),
        );

        let mut pause_commit = phase_hook.pause(1, OperationPhase::AfterReplace);
        let writer_commit = writer.clone();
        let task_commit = tokio::spawn(async move {
            writer_commit
                .commit(ClientOperationKey(1), committed_cfg, 0)
                .await
        });

        pause_commit.wait_reached().await;

        let writer_mutate = writer.clone();
        let task_mutate = tokio::spawn(async move {
            writer_mutate
                .mutate(ClientOperationKey(2), add("mutated_model"))
                .await
        });

        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // While commit is held at AfterReplace:
        // Mutate has entered the mailbox, but cannot be admitted yet because the actor processes sequentially
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert!(!task_commit.is_finished());
        assert!(!task_mutate.is_finished());

        pause_commit.release();

        let (_, rev_commit) = task_commit.await.unwrap().expect("commit succeeds");
        let (_, rev_mutate, _) = task_mutate.await.unwrap().expect("mutate succeeds");

        assert_eq!((rev_commit, rev_mutate), (1, 2));

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 2);
        assert!(snapshot.config().models.contains_key("committed_model"));
        assert!(snapshot.config().models.contains_key("mutated_model"));

        let core_guard = core.lock().await;
        assert!(core_guard.config().models.contains_key("committed_model"));
        assert!(core_guard.config().models.contains_key("mutated_model"));
        drop(core_guard);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_reload_then_mutate_uses_loaded_config_without_reload_replace() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, _core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        // Prepare isolated new config on disk
        let mut disk_cfg = config();
        disk_cfg.models.insert(
            "reloaded_model".to_string(),
            toml::from_str("context_window = 4096\nmax_output_tokens = 512\n")
                .expect("model config"),
        );
        crate::config_edit::write_full_config(&path, &disk_cfg).expect("write disk config");

        let mut pause_reload = phase_hook.pause(1, OperationPhase::AfterLoad);
        let writer_reload = writer.clone();
        let task_reload =
            tokio::spawn(async move { writer_reload.reload(ClientOperationKey(1)).await });

        pause_reload.wait_reached().await;

        let writer_mutate = writer.clone();
        let task_mutate = tokio::spawn(async move {
            writer_mutate
                .mutate(ClientOperationKey(2), add("mutated_model"))
                .await
        });

        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // While reload is held at AfterLoad:
        // Mutate has entered the mailbox, but cannot be admitted yet because the actor processes sequentially
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert!(!task_reload.is_finished());
        assert!(!task_mutate.is_finished());

        pause_reload.release();

        let (_, loaded_cfg) = task_reload.await.unwrap().expect("reload succeeds");
        assert!(loaded_cfg.models.contains_key("reloaded_model"));

        let (_, rev_mutate, _) = task_mutate.await.unwrap().expect("mutate succeeds");
        assert_eq!(rev_mutate, 2);

        // Reload itself performs zero replacements; only mutate replaced once
        assert_eq!(persistence.call_count(), 1);

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 2);
        assert!(snapshot.config().models.contains_key("reloaded_model"));
        assert!(snapshot.config().models.contains_key("mutated_model"));

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_mutate_then_stale_base_commit_is_rejected_not_overwritten() {
        // D2 boundary, superseded by D3 form B, 2026-09-19:
        // a whole-config candidate prepared before a mutation and committed after
        // it publishes is no longer an intentional overwrite — the actor rejects
        // it with CommitBaseConflict and the caller retries from the new
        // generation. Full replacement stays legal only with a fresh base
        // (the client side of that base is wired end to end now that the
        // residual closed: /admin/config/snapshot hands out body + generation
        // together, /admin/config/update requires the base).
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        // Prepare a whole Config candidate before mutation that includes a provider change
        // and deliberately lacks the model change to be introduced by the mutation.
        let mut commit_candidate = config();
        commit_candidate.providers.insert(
            "commit_provider".to_string(),
            toml::from_str(
                "api_key_env = \"TEST_KEY\"\nopenai_chat = { url = \"https://example.test/chat\" }\n",
            )
            .expect("provider config"),
        );
        assert!(!commit_candidate.models.contains_key("mutated_model"));

        // Pause mutation at a deterministic phase (Prepared)
        let mut pause_mutate = phase_hook.pause(1, OperationPhase::Prepared);
        let writer_mutate = writer.clone();
        let task_mutate = tokio::spawn(async move {
            writer_mutate
                .mutate(ClientOperationKey(1), add("mutated_model"))
                .await
        });

        pause_mutate.wait_reached().await;

        // Enqueue the stale-base Commit while the mutation is still paused, so
        // the actor admits it strictly after the mutation publishes.
        let writer_commit = writer.clone();
        let commit_candidate_clone = commit_candidate.clone();
        let task_commit = tokio::spawn(async move {
            writer_commit
                .commit(ClientOperationKey(2), commit_candidate_clone, 0)
                .await
        });

        // Wait until Commit has entered the mailbox
        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // Confirm the later Commit is in the mailbox before releasing mutation:
        // Commit has entered the mailbox, but cannot be admitted yet because the actor processes sequentially
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert!(!task_mutate.is_finished());
        assert!(!task_commit.is_finished());

        // Release the mutation
        pause_mutate.release();

        // Assert mutation first publishes its revision (revision 1)
        let (_, rev_mutate, _) = task_mutate.await.unwrap().expect("mutate succeeds");
        assert_eq!(rev_mutate, 1);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Published), 0);
        assert_eq!(persistence.call_count(), 1);

        let mid_snapshot = generation.load();
        assert_eq!(mid_snapshot.revision(), 1);
        assert!(mid_snapshot.config().models.contains_key("mutated_model"));
        assert!(
            !mid_snapshot
                .config()
                .providers
                .contains_key("commit_provider")
        );

        let mid_disk = Config::load(&path).expect("load intermediate disk config");
        assert!(mid_disk.models.contains_key("mutated_model"));
        assert!(!mid_disk.providers.contains_key("commit_provider"));

        let mid_core = core.lock().await;
        assert!(mid_core.config().models.contains_key("mutated_model"));
        assert!(!mid_core.config().providers.contains_key("commit_provider"));
        drop(mid_core);

        // The stale-base commit is rejected fail-stop: its candidate derived
        // from revision 0, but the actor is at 1 now (form B).
        assert_eq!(
            task_commit.await.unwrap(),
            Err(WriterError::CommitBaseConflict {
                expected: 0,
                actual: 1
            }),
            "stale base must conflict, never overwrite"
        );
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Published), 0);
        assert_eq!(
            persistence.call_count(),
            1,
            "the rejected commit performs no replacement"
        );

        // The mutation's change survives intact on generation and disk.
        let survived = generation.load();
        assert_eq!(survived.revision(), 1);
        assert!(survived.config().models.contains_key("mutated_model"));
        assert!(!survived.config().providers.contains_key("commit_provider"));
        let disk_after_conflict = Config::load(&path).expect("load disk config");
        assert!(disk_after_conflict.models.contains_key("mutated_model"));
        assert!(
            !disk_after_conflict
                .providers
                .contains_key("commit_provider")
        );

        // Documented retry path: rebuild the candidate from the current
        // generation and commit with the fresh base; both changes coexist.
        let mut retry_candidate = survived.config().clone();
        retry_candidate.providers.insert(
            "commit_provider".to_string(),
            toml::from_str(
                "api_key_env = \"TEST_KEY\"\nopenai_chat = { url = \"https://example.test/chat\" }\n",
            )
            .expect("provider config"),
        );
        let (_, rev_retry) = writer
            .commit(ClientOperationKey(3), retry_candidate, survived.revision())
            .await
            .expect("retry with fresh base");
        assert_eq!(rev_retry, 2);
        let final_snapshot = generation.load();
        assert_eq!(final_snapshot.revision(), 2);
        assert!(final_snapshot.config().models.contains_key("mutated_model"));
        assert!(
            final_snapshot
                .config()
                .providers
                .contains_key("commit_provider")
        );

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_not_replaced_keeps_state_unchanged_and_replays() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        persistence.queue_outcome(ReplaceOutcome::NotReplaced(
            "simulated write failure before replace".to_string(),
        ));
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let initial_bytes = std::fs::read(&path).expect("read initial disk config");

        let key = ClientOperationKey(101);
        let res1 = writer.mutate(key, add("failed_model")).await;
        assert!(
            matches!(&res1, Err(WriterError::FailedBeforeReplace(msg)) if msg.contains("simulated write failure"))
        );

        // Assert disk bytes, parsed config, core, generation, revision remain unchanged
        let disk_bytes = std::fs::read(&path).expect("read disk config after failed replace");
        assert_eq!(disk_bytes, initial_bytes);

        let disk_cfg = Config::load(&path).expect("load disk");
        assert!(!disk_cfg.models.contains_key("failed_model"));

        let core_guard = core.lock().await;
        assert!(!core_guard.config().models.contains_key("failed_model"));
        drop(core_guard);

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 0);
        assert!(!snapshot.config().models.contains_key("failed_model"));
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);

        // Replay with identical key and identity
        let res2 = writer.mutate(key, add("failed_model")).await;
        assert_eq!(res1, res2);

        // Replay did not execute persistence replacement again
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 0);
        let disk_bytes_replay = std::fs::read(&path).expect("read disk config after replay");
        assert_eq!(disk_bytes_replay, initial_bytes);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_post_replacement_unknown_updates_disk_and_enters_fail_stop() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        persistence.queue_outcome(TestPersistenceOutcome::PostReplacementUnknown(
            "simulated post-replacement failure".to_string(),
        ));
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let initial_bytes = std::fs::read(&path).expect("read initial config bytes");

        let key = ClientOperationKey(101);
        let res = writer.mutate(key, add("post_unknown_model")).await;
        assert_eq!(res, Err(WriterError::RetrySafetyUnknown));

        // Prove bytes on disk ARE changed and match new mutated config
        let disk_bytes =
            std::fs::read(&path).expect("read disk bytes after post-replacement failure");
        assert_ne!(disk_bytes, initial_bytes);
        let disk_cfg =
            Config::load(&path).expect("load disk config after post-replacement failure");
        assert!(disk_cfg.models.contains_key("post_unknown_model"));

        assert_eq!(persistence.call_count(), 1);

        // Core and generation remain uncommitted / unpublished
        let core_guard = core.lock().await;
        assert!(
            !core_guard
                .config()
                .models
                .contains_key("post_unknown_model")
        );
        drop(core_guard);

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 0);
        assert!(!snapshot.config().models.contains_key("post_unknown_model"));
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);

        // Actor enters fail-stop
        assert!(writer.fatal_config_state().is_failed());
        writer.fatal_config_state().wait().await;

        // Writer task exits
        task.await.expect("actor join must succeed");

        // Subsequent operations are rejected with FailedStop
        let next_mutate = writer
            .mutate(ClientOperationKey(102), add("future_model"))
            .await;
        assert_eq!(next_mutate, Err(WriterError::FailedStop));

        let next_commit = writer.commit(ClientOperationKey(103), config(), 0).await;
        assert_eq!(next_commit, Err(WriterError::FailedStop));

        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));
    }

    #[tokio::test]
    async fn phase_control_post_replacement_join_error_updates_disk_and_enters_fail_stop() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        persistence.queue_outcome(TestPersistenceOutcome::PostReplacementPanic(
            "simulated blocking persistence task join error/panic".to_string(),
        ));
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let initial_bytes = std::fs::read(&path).expect("read initial config bytes");

        let key = ClientOperationKey(101);
        let res = writer.mutate(key, add("post_join_error_model")).await;

        // Current operation sees RetrySafetyUnknown, never false FailedBeforeReplace
        assert_eq!(res, Err(WriterError::RetrySafetyUnknown));
        assert!(!matches!(res, Err(WriterError::FailedBeforeReplace(_))));

        // Controlled seam establishes replacement outcome: disk bytes ARE changed and match new mutated config
        let disk_bytes =
            std::fs::read(&path).expect("read disk bytes after post-replacement join error");
        assert_ne!(disk_bytes, initial_bytes);
        let disk_cfg =
            Config::load(&path).expect("load disk config after post-replacement join error");
        assert!(disk_cfg.models.contains_key("post_join_error_model"));

        assert_eq!(persistence.call_count(), 1);

        // Core and generation remain uncommitted / unpublished
        let core_guard = core.lock().await;
        assert!(
            !core_guard
                .config()
                .models
                .contains_key("post_join_error_model")
        );
        drop(core_guard);

        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 0);
        assert!(
            !snapshot
                .config()
                .models
                .contains_key("post_join_error_model")
        );
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);

        // Actor enters fail-stop
        assert!(writer.fatal_config_state().is_failed());
        writer.fatal_config_state().wait().await;

        // Writer task exits
        task.await.expect("actor join must succeed");

        // Subsequent operations are rejected with FailedStop
        let next_mutate = writer
            .mutate(ClientOperationKey(102), add("future_model"))
            .await;
        assert_eq!(next_mutate, Err(WriterError::FailedStop));

        let next_commit = writer.commit(ClientOperationKey(103), config(), 0).await;
        assert_eq!(next_commit, Err(WriterError::FailedStop));

        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));
    }

    #[tokio::test]
    async fn phase_control_injected_replaced_performs_replacement() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        persistence.queue_outcome(TestPersistenceOutcome::Replaced);
        let (_temp, writer, generation, _core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let initial_bytes = std::fs::read(&path).expect("read initial config bytes");

        let res = writer
            .mutate(ClientOperationKey(101), add("injected_replaced_model"))
            .await;
        assert!(res.is_ok());

        // Prove bytes on disk ARE changed and match new config
        let disk_bytes = std::fs::read(&path).expect("read disk bytes after injected replaced");
        assert_ne!(disk_bytes, initial_bytes);
        let disk_cfg = Config::load(&path).expect("load disk config after injected replaced");
        assert!(disk_cfg.models.contains_key("injected_replaced_model"));

        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 1);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_publication_window_blocks_ack_and_next_prepare_until_released() {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let mut pause_1 = phase_hook.pause(1, OperationPhase::AfterReplace);
        let writer_1 = writer.clone();
        let task_1 =
            tokio::spawn(
                async move { writer_1.mutate(ClientOperationKey(1), add("model_1")).await },
            );

        pause_1.wait_reached().await;

        // While paused at AfterReplace:
        // Disk replacement has executed
        assert_eq!(persistence.call_count(), 1);
        let disk_cfg = Config::load(&path).expect("load disk config while paused at AfterReplace");
        assert!(disk_cfg.models.contains_key("model_1"));
        // Generation is still old
        assert_eq!(generation.load().revision(), 0);
        assert!(!generation.load().config().models.contains_key("model_1"));
        // No ack has been sent for operation 1
        assert!(!task_1.is_finished());

        // Enqueue operation 2
        let writer_2 = writer.clone();
        let task_2 =
            tokio::spawn(
                async move { writer_2.mutate(ClientOperationKey(2), add("model_2")).await },
            );

        phase_hook.wait_enqueued(ClientOperationKey(2)).await;

        // Prove no ack for 1, operation 2 has entered mailbox but NOT reached Prepared
        assert!(!task_1.is_finished());
        assert!(!task_2.is_finished());
        assert_eq!(phase_hook.enqueued_count(ClientOperationKey(2)), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Prepared), 0);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::BeforeAck), 0);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);

        // Core-lock contention test:
        // Acquire core mutex before releasing AfterReplace
        let core_guard = core.lock().await;
        assert!(!core_guard.config().models.contains_key("model_1"));

        // Release the phase pause so actor advances toward publish
        pause_1.release();

        // Use an explicit test-only phase/notification to prove it reaches BeforeCorePublish
        // or is blocked awaiting core (without sleep)
        phase_hook
            .wait_phase(1, OperationPhase::BeforeCorePublish)
            .await;

        // Assert no Published/ack while lock is held
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::BeforeAck), 0);
        assert!(!task_1.is_finished());
        assert_eq!(generation.load().revision(), 0);
        assert!(!generation.load().config().models.contains_key("model_1"));
        assert!(!core_guard.config().models.contains_key("model_1"));

        // Release core and assert eventual publish/ack
        drop(core_guard);

        let res1 = task_1.await.unwrap();
        let res2 = task_2.await.unwrap();

        assert!(res1.is_ok());
        assert!(res2.is_ok());

        assert_eq!(generation.load().revision(), 2);
        assert!(generation.load().config().models.contains_key("model_1"));
        assert!(generation.load().config().models.contains_key("model_2"));
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Published), 1);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_before_generation_publish_observes_core_updated_without_holding_core_lock()
     {
        let phase_hook = PhaseHook::new();
        let (_temp, writer, generation, core, task, _path) =
            writer_with_controls(config(), Some(phase_hook.clone()), None);

        let mut pause = phase_hook.pause(1, OperationPhase::BeforeGenerationPublish);
        let writer_clone = writer.clone();
        let task_mutate = tokio::spawn(async move {
            writer_clone
                .mutate(ClientOperationKey(1), add("interim_model"))
                .await
        });

        pause.wait_reached().await;

        // While paused at BeforeGenerationPublish:
        // Core lock must NOT be held across await, so we can lock it here without deadlocking!
        let core_has_model = {
            let core_guard = core.lock().await;
            core_guard.config().models.contains_key("interim_model")
        };
        assert!(core_has_model, "core is updated before generation publish");

        // Generation has NOT yet been published
        let snapshot = generation.load();
        assert_eq!(snapshot.revision(), 0);
        assert!(!snapshot.config().models.contains_key("interim_model"));

        pause.release();

        let res = task_mutate.await.unwrap();
        assert!(res.is_ok());

        let snapshot_after = generation.load();
        assert_eq!(snapshot_after.revision(), 1);
        assert!(snapshot_after.config().models.contains_key("interim_model"));

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn phase_control_dropped_ordinary_ack_preserves_mutation_and_replays_with_alive_senders()
    {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            config(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let sender_clone1 = writer.clone();
        let sender_clone2 = writer.clone();

        let mut pause_before_ack = phase_hook.pause(1, OperationPhase::BeforeAck);
        let key = ClientOperationKey(101);
        let mutation = add("model-dropped-ack");

        let writer_caller = writer.clone();
        let mutation_caller = mutation.clone();
        let mut caller_task =
            tokio::spawn(async move { writer_caller.mutate(key, mutation_caller).await });

        pause_before_ack.wait_reached().await;

        assert!(!caller_task.is_finished());
        assert_eq!(phase_hook.phase_count(1, OperationPhase::BeforeAck), 1);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 1);
        assert_eq!(persistence.call_count(), 1);

        caller_task.abort();
        let join_err = (&mut caller_task)
            .await
            .expect_err("calling task must be aborted");
        assert!(
            join_err.is_cancelled(),
            "confirmed cancellation of calling task"
        );

        pause_before_ack.release();

        let resubmit_res = writer.mutate(key, mutation.clone()).await;

        let expected_receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 99,
                sequence: 1,
            },
        };
        let (replay_receipt, replay_revision, replay_msg) = resubmit_res
            .expect("resubmission of identical key and payload must succeed via replay");
        assert_eq!(replay_receipt, expected_receipt);
        assert_eq!(replay_revision, 1);

        let looked_up = writer
            .lookup(expected_receipt)
            .await
            .expect("retained receipt lookup must succeed");
        assert_eq!(
            looked_up,
            TerminalResponse::Mutate {
                revision: replay_revision,
                message: replay_msg.clone(),
            }
        );

        assert_eq!(
            persistence.call_count(),
            1,
            "exactly one real replacement executed"
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Published),
            1,
            "exactly one publication executed"
        );
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Published), 0);

        assert_eq!(
            replay_receipt.id.sequence, 1,
            "replayed receipt preserves original allocated sequence"
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Admitted),
            1,
            "no new sequence admitted on replay"
        );
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Admitted), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);

        let disk_cfg = Config::load(&path).expect("load disk config after replay");
        assert!(
            disk_cfg.models.contains_key("model-dropped-ack"),
            "disk preserves successful mutation"
        );

        {
            let core_guard = core.lock().await;
            assert!(
                core_guard.config().models.contains_key("model-dropped-ack"),
                "core preserves successful mutation"
            );
        }

        let gen_snapshot = generation.load();
        assert_eq!(gen_snapshot.revision(), 1);
        assert!(
            gen_snapshot
                .config()
                .models
                .contains_key("model-dropped-ack"),
            "generation preserves successful mutation"
        );

        let fresh_key = ClientOperationKey(102);
        let (fresh_receipt, fresh_revision, _) = writer
            .mutate(fresh_key, add("fresh-model"))
            .await
            .expect("fresh mutation must succeed");
        assert_eq!(fresh_receipt.id.sequence, expected_receipt.id.sequence + 1);
        assert_eq!(fresh_revision, replay_revision + 1);

        let shutdown_res = tokio::time::timeout(Duration::from_secs(2), writer.shutdown()).await;
        assert_eq!(
            shutdown_res.expect("shutdown must not time out"),
            Ok(()),
            "normal shutdown completes successfully"
        );

        let join_res = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(
            join_res.expect("task join must not time out").is_ok(),
            "actor task join completes cleanly"
        );

        assert_eq!(
            sender_clone1
                .mutate(ClientOperationKey(103), add("late-model"))
                .await,
            Err(WriterError::ServiceShuttingDown)
        );
        drop(sender_clone1);
        drop(sender_clone2);
    }

    async fn assert_shutdown_during_persistence(phase: OperationPhase) {
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (marker_tx, marker_rx) = oneshot::channel();
        let test_hook = ShutdownTestHook::new_with_marker(started_tx, resume_rx, marker_tx);

        let (_temp, writer, generation, core, task, path) = writer_with_all_controls(
            config(),
            Some(test_hook),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );

        let mut pause = phase_hook.pause(1, phase);
        let key1 = ClientOperationKey(101);
        let mutation1 = add("model-persisted");
        let w_mutate = writer.clone();
        let task_mutate = tokio::spawn(async move { w_mutate.mutate(key1, mutation1).await });

        pause.wait_reached().await;

        let w_shutdown = writer.clone();
        let task_shutdown = tokio::spawn(async move { w_shutdown.shutdown().await });

        started_rx.await.expect("coordinator started");
        resume_tx.send(()).expect("resume send");
        marker_rx.await.expect("marker sent");

        let key2 = ClientOperationKey(102);
        let mutation2 = add("model-unadmitted");
        let w_unadmitted = writer.clone();
        let task_unadmitted =
            tokio::spawn(async move { w_unadmitted.mutate(key2, mutation2).await });
        phase_hook.wait_enqueued(key2).await;

        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Closing), 0);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Prepared), 0);
        assert_eq!(writer.lifecycle_state(), RUNNING);
        if phase == OperationPhase::BeforeReplace {
            assert_eq!(persistence.call_count(), 0);
        } else {
            assert_eq!(persistence.call_count(), 1);
        }

        pause.release();

        let (receipt1, rev1, _) = task_mutate.await.expect("mutate join").expect("mutate ok");
        assert_eq!(rev1, 1);
        assert_eq!(receipt1.id.sequence, 1);

        let res_unadmitted = task_unadmitted.await.expect("unadmitted join");
        assert_eq!(res_unadmitted, Err(WriterError::ServiceShuttingDown));

        let res_shutdown = task_shutdown.await.expect("shutdown join");
        assert_eq!(res_shutdown, Ok(()));
        task.await.expect("actor join");

        let history = phase_hook.history();
        let published_idx = history
            .iter()
            .position(|(id, p)| id.sequence == 1 && *p == OperationPhase::Published)
            .expect("published in history");
        let before_ack_idx = history
            .iter()
            .position(|(id, p)| id.sequence == 1 && *p == OperationPhase::BeforeAck)
            .expect("before_ack in history");
        let closing_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Closing)
            .expect("closing in history");
        let drained_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Drained)
            .expect("drained in history");
        let before_actor_exit_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::BeforeActorExit)
            .expect("before actor exit in history");

        assert!(published_idx < closing_idx);
        assert!(before_ack_idx < closing_idx);
        assert!(closing_idx < drained_idx);
        assert!(drained_idx < before_actor_exit_idx);

        let disk_cfg = Config::load(&path).expect("load disk config");
        assert!(disk_cfg.models.contains_key("model-persisted"));
        assert!(!disk_cfg.models.contains_key("model-unadmitted"));

        {
            let core_guard = core.lock().await;
            assert!(core_guard.config().models.contains_key("model-persisted"));
            assert!(!core_guard.config().models.contains_key("model-unadmitted"));
        }

        let gen_snapshot = generation.load();
        assert_eq!(gen_snapshot.revision(), 1);
        assert!(gen_snapshot.config().models.contains_key("model-persisted"));
        assert!(
            !gen_snapshot
                .config()
                .models
                .contains_key("model-unadmitted")
        );

        assert_eq!(persistence.call_count(), 1);
        assert_eq!(persistence.load_call_count(), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Prepared), 1);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Prepared), 0);
        assert_eq!(phase_hook.phase_count(2, OperationPhase::Admitted), 0);

        let late_res = writer
            .mutate(ClientOperationKey(103), add("model-late"))
            .await;
        assert_eq!(late_res, Err(WriterError::ServiceShuttingDown));
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(persistence.load_call_count(), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Prepared), 1);
        assert_eq!(phase_hook.phase_count(3, OperationPhase::Admitted), 0);
        assert_eq!(writer.lifecycle_state(), CLOSED);
    }

    #[tokio::test]
    async fn phase_control_shutdown_during_persistence_before_replace() {
        assert_shutdown_during_persistence(OperationPhase::BeforeReplace).await;
    }

    #[tokio::test]
    async fn phase_control_shutdown_during_persistence_after_replace() {
        assert_shutdown_during_persistence(OperationPhase::AfterReplace).await;
    }

    async fn assert_mailbox_saturation_with_marker_ordering(
        pre_marker_count: usize,
        post_marker_count: usize,
    ) {
        use std::future::Future;

        assert_eq!(pre_marker_count + 1 + post_marker_count, 8);
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (marker_tx, marker_rx) = oneshot::channel();
        let test_hook = ShutdownTestHook::new_with_marker(started_tx, resume_rx, marker_tx);

        let (_temp, writer, generation, core, task, path) = writer_with_all_controls(
            config(),
            Some(test_hook),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );
        let alive_clone = writer.clone();

        let mut pause = phase_hook.pause(1, OperationPhase::Prepared);
        let key_admitted = ClientOperationKey(100);
        let w_admitted = writer.clone();
        let task_admitted = tokio::spawn(async move {
            w_admitted
                .mutate(key_admitted, add("model-admitted-0"))
                .await
        });
        pause.wait_reached().await;

        let mut pre_marker_tasks = Vec::new();
        for i in 0..pre_marker_count {
            let key = ClientOperationKey(101 + i as u128);
            let model_id = format!("model-pre-{i}");
            let w = writer.clone();
            let t = tokio::spawn(async move { w.mutate(key, add(&model_id)).await });
            phase_hook.wait_enqueued(key).await;
            pre_marker_tasks.push(t);
        }

        let w_shutdown = writer.clone();
        let task_shutdown = tokio::spawn(async move { w_shutdown.shutdown().await });
        started_rx.await.expect("coordinator started");
        resume_tx.send(()).expect("resume send");
        marker_rx.await.expect("marker sent");

        let mut post_marker_tasks = Vec::new();
        for i in 0..post_marker_count {
            let key = ClientOperationKey(201 + i as u128);
            let model_id = format!("model-post-{i}");
            let w = writer.clone();
            let t = tokio::spawn(async move { w.mutate(key, add(&model_id)).await });
            phase_hook.wait_enqueued(key).await;
            post_marker_tasks.push(t);
        }

        let waiting_key = ClientOperationKey(301);
        let w_waiting = writer.clone();
        let mut waiting_fut =
            Box::pin(async move { w_waiting.mutate(waiting_key, add("model-waiting")).await });
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(waiting_fut.as_mut().poll(&mut cx).is_pending());

        let task_waiting = tokio::spawn(waiting_fut);

        assert_eq!(writer.lifecycle_state(), RUNNING);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Closing), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Prepared), 1);
        assert_eq!(persistence.call_count(), 0);

        pause.release();

        let (receipt_admitted, rev_admitted, _) = task_admitted
            .await
            .expect("admitted join")
            .expect("admitted ok");
        assert_eq!(rev_admitted, 1);
        assert_eq!(receipt_admitted.id.sequence, 1);

        for (i, t) in pre_marker_tasks.into_iter().enumerate() {
            let expected_seq = 2 + i as u64;
            let (receipt_pre, rev_pre, _) =
                t.await.expect("pre-marker join").expect("pre-marker ok");
            assert_eq!(receipt_pre.id.sequence, expected_seq);
            assert_eq!(rev_pre, expected_seq);
        }

        for t in post_marker_tasks {
            let res = t.await.expect("post-marker join");
            assert!(
                matches!(
                    res,
                    Err(WriterError::ServiceShuttingDown | WriterError::MailboxClosed)
                ),
                "expected ServiceShuttingDown or MailboxClosed, got {res:?}"
            );
        }

        let res_waiting = task_waiting.await.expect("waiting send join");
        assert!(
            matches!(
                res_waiting,
                Err(WriterError::ServiceShuttingDown | WriterError::MailboxClosed)
            ),
            "expected ServiceShuttingDown or MailboxClosed, got {res_waiting:?}"
        );

        let res_shutdown = task_shutdown.await.expect("shutdown join");
        assert_eq!(res_shutdown, Ok(()));
        task.await.expect("actor join");

        let total_admitted = 1 + pre_marker_count;

        let history = phase_hook.history();
        let closing_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Closing)
            .expect("closing in history");
        let drained_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Drained)
            .expect("drained in history");
        let before_actor_exit_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::BeforeActorExit)
            .expect("before actor exit in history");

        assert!(closing_idx < drained_idx);
        assert!(drained_idx < before_actor_exit_idx);

        for seq in 1..=total_admitted as u64 {
            let published_idx = history
                .iter()
                .position(|(id, p)| id.sequence == seq && *p == OperationPhase::Published)
                .expect("published in history");
            let before_ack_idx = history
                .iter()
                .position(|(id, p)| id.sequence == seq && *p == OperationPhase::BeforeAck)
                .expect("before_ack in history");
            assert!(published_idx < closing_idx);
            assert!(before_ack_idx < closing_idx);
        }

        assert_eq!(persistence.call_count(), total_admitted);
        assert_eq!(persistence.load_call_count(), 0);
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Admitted),
            total_admitted
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Prepared),
            total_admitted
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Published),
            total_admitted
        );
        for seq in (total_admitted as u64 + 1)..=20 {
            assert_eq!(phase_hook.phase_count(seq, OperationPhase::Admitted), 0);
            assert_eq!(phase_hook.phase_count(seq, OperationPhase::Prepared), 0);
        }

        let disk_cfg = Config::load(&path).expect("load disk config");
        assert_eq!(disk_cfg.models.len(), total_admitted);
        assert!(disk_cfg.models.contains_key("model-admitted-0"));
        for i in 0..pre_marker_count {
            assert!(disk_cfg.models.contains_key(&format!("model-pre-{i}")));
        }
        for i in 0..post_marker_count {
            assert!(!disk_cfg.models.contains_key(&format!("model-post-{i}")));
        }
        assert!(!disk_cfg.models.contains_key("model-waiting"));

        {
            let core_guard = core.lock().await;
            assert_eq!(core_guard.config().models.len(), total_admitted);
            assert!(core_guard.config().models.contains_key("model-admitted-0"));
            for i in 0..pre_marker_count {
                assert!(
                    core_guard
                        .config()
                        .models
                        .contains_key(&format!("model-pre-{i}"))
                );
            }
            for i in 0..post_marker_count {
                assert!(
                    !core_guard
                        .config()
                        .models
                        .contains_key(&format!("model-post-{i}"))
                );
            }
            assert!(!core_guard.config().models.contains_key("model-waiting"));
        }

        let gen_snapshot = generation.load();
        assert_eq!(gen_snapshot.revision(), total_admitted as u64);
        assert_eq!(gen_snapshot.config().models.len(), total_admitted);
        assert!(
            gen_snapshot
                .config()
                .models
                .contains_key("model-admitted-0")
        );
        for i in 0..pre_marker_count {
            assert!(
                gen_snapshot
                    .config()
                    .models
                    .contains_key(&format!("model-pre-{i}"))
            );
        }
        for i in 0..post_marker_count {
            assert!(
                !gen_snapshot
                    .config()
                    .models
                    .contains_key(&format!("model-post-{i}"))
            );
        }
        assert!(!gen_snapshot.config().models.contains_key("model-waiting"));

        assert_eq!(alive_clone.lifecycle_state(), CLOSED);
        drop(alive_clone);

        let late_res = writer
            .mutate(ClientOperationKey(999), add("model-late"))
            .await;
        assert_eq!(late_res, Err(WriterError::ServiceShuttingDown));
        assert_eq!(persistence.call_count(), total_admitted);
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Prepared),
            total_admitted
        );
    }

    #[tokio::test]
    async fn phase_control_mailbox_saturation_marker_precedes_competing_requests() {
        assert_mailbox_saturation_with_marker_ordering(2, 5).await;
    }

    #[tokio::test]
    async fn phase_control_mailbox_saturation_marker_follows_competing_requests() {
        assert_mailbox_saturation_with_marker_ordering(5, 2).await;
    }

    #[tokio::test]
    async fn phase_control_mailbox_saturation_pending_marker_with_canceled_caller() {
        use std::future::Future;

        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (started_tx, started_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let (marker_tx, marker_rx) = oneshot::channel();
        let (send_pending_tx, send_pending_rx) = oneshot::channel();
        let test_hook = ShutdownTestHook::new_with_marker_and_send_pending(
            started_tx,
            resume_rx,
            marker_tx,
            send_pending_tx,
        );

        let (_temp, writer, generation, core, task, path) = writer_with_all_controls(
            config(),
            Some(test_hook),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );
        let alive_clone = writer.clone();

        let mut pause = phase_hook.pause(1, OperationPhase::Prepared);
        let key_admitted = ClientOperationKey(100);
        let w_admitted = writer.clone();
        let task_admitted = tokio::spawn(async move {
            w_admitted
                .mutate(key_admitted, add("model-admitted-0"))
                .await
        });
        pause.wait_reached().await;

        let mut pre_marker_tasks = Vec::new();
        for i in 0..8 {
            let key = ClientOperationKey(101 + i as u128);
            let model_id = format!("model-pre-{i}");
            let w = writer.clone();
            let t = tokio::spawn(async move { w.mutate(key, add(&model_id)).await });
            phase_hook.wait_enqueued(key).await;
            pre_marker_tasks.push(t);
        }

        let w_shutdown = writer.clone();
        let task_shutdown = tokio::spawn(async move { w_shutdown.shutdown().await });
        started_rx.await.expect("coordinator started");
        resume_tx.send(()).expect("resume send");
        send_pending_rx
            .await
            .expect("shutdown actual send polled pending on full mailbox");

        let waiting_key = ClientOperationKey(301);
        let w_waiting = writer.clone();
        let mut waiting_fut =
            Box::pin(async move { w_waiting.mutate(waiting_key, add("model-waiting")).await });
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(waiting_fut.as_mut().poll(&mut cx).is_pending());

        let task_waiting = tokio::spawn(waiting_fut);

        task_shutdown.abort();
        let abort_err = task_shutdown
            .await
            .expect_err("initiating shutdown task was cancelled");
        assert!(abort_err.is_cancelled());

        assert_eq!(writer.lifecycle_state(), RUNNING);
        assert_eq!(phase_hook.phase_count(1, OperationPhase::Published), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Closing), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Prepared), 1);
        assert_eq!(persistence.call_count(), 0);

        pause.release();

        let (receipt_admitted, rev_admitted, _) = task_admitted
            .await
            .expect("admitted join")
            .expect("admitted ok");
        assert_eq!(rev_admitted, 1);
        assert_eq!(receipt_admitted.id.sequence, 1);

        for (i, t) in pre_marker_tasks.into_iter().enumerate() {
            let expected_seq = 2 + i as u64;
            let (receipt_pre, rev_pre, _) =
                t.await.expect("pre-marker join").expect("pre-marker ok");
            assert_eq!(receipt_pre.id.sequence, expected_seq);
            assert_eq!(rev_pre, expected_seq);
        }

        marker_rx.await.expect("marker sent");

        let res_waiting = task_waiting.await.expect("waiting send join");
        assert!(
            matches!(
                res_waiting,
                Err(WriterError::ServiceShuttingDown | WriterError::MailboxClosed)
            ),
            "expected ServiceShuttingDown or MailboxClosed, got {res_waiting:?}"
        );

        let total_admitted = 1 + 8;

        let res_shutdown = writer.shutdown().await;
        assert_eq!(res_shutdown, Ok(()));
        task.await.expect("actor join");

        let history = phase_hook.history();
        let closing_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Closing)
            .expect("closing in history");
        let drained_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::Drained)
            .expect("drained in history");
        let before_actor_exit_idx = history
            .iter()
            .position(|(_, p)| *p == OperationPhase::BeforeActorExit)
            .expect("before actor exit in history");

        assert!(closing_idx < drained_idx);
        assert!(drained_idx < before_actor_exit_idx);

        for seq in 1..=total_admitted as u64 {
            let published_idx = history
                .iter()
                .position(|(id, p)| id.sequence == seq && *p == OperationPhase::Published)
                .expect("published in history");
            let before_ack_idx = history
                .iter()
                .position(|(id, p)| id.sequence == seq && *p == OperationPhase::BeforeAck)
                .expect("before_ack in history");
            assert!(published_idx < closing_idx);
            assert!(before_ack_idx < closing_idx);
        }

        assert_eq!(persistence.call_count(), total_admitted);
        assert_eq!(persistence.load_call_count(), 0);
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Admitted),
            total_admitted
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Prepared),
            total_admitted
        );
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Published),
            total_admitted
        );
        for seq in (total_admitted as u64 + 1)..=20 {
            assert_eq!(phase_hook.phase_count(seq, OperationPhase::Admitted), 0);
            assert_eq!(phase_hook.phase_count(seq, OperationPhase::Prepared), 0);
        }

        let disk_cfg = Config::load(&path).expect("load disk config");
        assert_eq!(disk_cfg.models.len(), total_admitted);
        assert!(disk_cfg.models.contains_key("model-admitted-0"));
        for i in 0..8 {
            assert!(disk_cfg.models.contains_key(&format!("model-pre-{i}")));
        }
        assert!(!disk_cfg.models.contains_key("model-waiting"));

        {
            let core_guard = core.lock().await;
            assert_eq!(core_guard.config().models.len(), total_admitted);
            assert!(core_guard.config().models.contains_key("model-admitted-0"));
            for i in 0..8 {
                assert!(
                    core_guard
                        .config()
                        .models
                        .contains_key(&format!("model-pre-{i}"))
                );
            }
            assert!(!core_guard.config().models.contains_key("model-waiting"));
        }

        let gen_snapshot = generation.load();
        assert_eq!(gen_snapshot.revision(), total_admitted as u64);
        assert_eq!(gen_snapshot.config().models.len(), total_admitted);
        assert!(
            gen_snapshot
                .config()
                .models
                .contains_key("model-admitted-0")
        );
        for i in 0..8 {
            assert!(
                gen_snapshot
                    .config()
                    .models
                    .contains_key(&format!("model-pre-{i}"))
            );
        }
        assert!(!gen_snapshot.config().models.contains_key("model-waiting"));

        assert_eq!(alive_clone.lifecycle_state(), CLOSED);
        drop(alive_clone);

        let late_res = writer
            .mutate(ClientOperationKey(999), add("model-late"))
            .await;
        assert_eq!(late_res, Err(WriterError::ServiceShuttingDown));
        assert_eq!(persistence.call_count(), total_admitted);
        assert_eq!(
            phase_hook.phase_count(0, OperationPhase::Prepared),
            total_admitted
        );
    }

    #[tokio::test]
    async fn unexpected_actor_failure_triggers_fatal_notification_rejects_later_mutation_and_fails_shutdown()
     {
        let phase_hook = PhaseHook::new();
        phase_hook.panic_on(
            1,
            OperationPhase::AfterReplace,
            "deterministic actor failure after replacement boundary",
        );
        let (_temp, writer, _generation, _core, task, _path) =
            writer_with_controls(config(), Some(phase_hook), None);

        let fatal = writer.fatal_config_state();
        assert!(!fatal.is_failed());

        let res = writer
            .mutate(ClientOperationKey(1), add("panicking-model"))
            .await;
        // In-flight operation sees error due to actor disconnect
        assert!(res.is_err());

        // 1. Proves unexpected actor failure triggers fatal notification
        assert!(fatal.is_failed());
        fatal.wait().await;

        // 2. Proves actor task terminates with panic
        let join_res = task.await;
        assert!(join_res.is_err() && join_res.unwrap_err().is_panic());

        // 3. Proves later mutation is rejected with FailedStop
        let later_mutate = writer
            .mutate(ClientOperationKey(2), add("later-model"))
            .await;
        assert_eq!(later_mutate, Err(WriterError::FailedStop));

        let later_commit = writer.commit(ClientOperationKey(3), config(), 0).await;
        assert_eq!(later_commit, Err(WriterError::FailedStop));

        let later_reload = writer.reload(ClientOperationKey(4)).await;
        assert!(matches!(later_reload, Err(WriterError::FailedStop)));

        // 4. Proves shutdown returns error rather than success
        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));

        // Shared shutdown completion memoizes error, repeated call also fails
        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));
    }

    #[tokio::test]
    async fn controlled_actor_abort_triggers_fatal_notification_rejects_mutation_and_fails_shutdown()
     {
        let (_temp, writer, _generation, _core, task, _path) =
            writer_with_controls(config(), None, None);
        let fatal = writer.fatal_config_state();
        assert!(!fatal.is_failed());

        // Abort the writer task
        task.abort();

        // Wait for fatal notification
        fatal.wait().await;
        assert!(fatal.is_failed());

        // Later mutation is rejected
        let later_mutate = writer
            .mutate(ClientOperationKey(201), add("aborted-later"))
            .await;
        assert_eq!(later_mutate, Err(WriterError::FailedStop));

        // Shutdown returns FailedStop
        assert_eq!(writer.shutdown().await, Err(WriterError::FailedStop));
    }

    #[tokio::test]
    async fn shutdown_coordinator_closed_fast_path_awaits_actor_join() {
        use std::future::Future;

        let phase_hook = PhaseHook::new();
        let (_temp, writer, _generation, _core, task, _path) =
            writer_with_controls(config(), Some(phase_hook.clone()), None);

        let mut pause = phase_hook.pause(0, OperationPhase::BeforeActorExit);

        let w_first = writer.clone();
        let task_first = tokio::spawn(async move { w_first.shutdown().await });

        pause.wait_reached().await;
        phase_hook
            .wait_phase(0, OperationPhase::ShutdownAckReceived)
            .await;

        // : actor 不再写 CLOSED——supervisor 是唯一产出者。
        // actor 暂停在 BeforeActorExit 时，lifecycle 停在 CLOSING（actor 准入闸门），
        // supervisor 尚未 join actor，故终态（CLOSED）和结果通道都还没写入。
        assert_eq!(writer.lifecycle_state(), CLOSING);
        assert!(writer.shared_shutdown_result().is_none());

        let w_second = writer.clone();
        let mut second_shutdown_fut = Box::pin(async move { w_second.shutdown().await });
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let is_pending = second_shutdown_fut.as_mut().poll(&mut cx).is_pending();
        assert!(
            is_pending,
            "expected second writer.shutdown() to be Pending while existing coordinator still awaits actor join"
        );

        pause.release();
        let first_res = task_first.await.expect("first shutdown task join");
        assert_eq!(first_res, Ok(()));
        let second_res = second_shutdown_fut.await;
        assert_eq!(second_res, Ok(()));
        task.await.expect("actor join");
        assert_eq!(writer.shared_shutdown_result(), Some(Ok(())));
    }

    /// 回归基线：关闭协调任务**不得**二次 poll 已经完成的 actor join future。
    ///
    /// 竞态形态：`ShutdownCoordinator::execute()` 把 supervisor 的 join future 用
    /// `pin!` 固定后，同时交给两个 `select!`（`_ = &mut join_fut`）和末尾一次
    /// `join_fut.await`。当"actor 发出 BeginShutdown 的 ack"与"actor 退出 ⇒ supervisor
    /// 完成"落在同一轮 poll 时，`select!` 随机选支；选中 join 支就意味着该 future 已被
    ///  poll 到 Ready，随后 `:823` 的 `await` 是对已完成 future 的二次 poll ⇒
    /// `` `async fn` resumed after completion `` panic。
    ///
    /// 为什么必须循环：这是**同拍竞态**，单轮命中率约 50%（隔离实例真实启停实测 6 中 3），
    /// 所以断言放在"若干轮里一次 panic 都不许出现"。
    ///
    /// 为什么不能只断言 `shutdown()` 的返回值：panic 时 supervisor 仍会把结论写进通道，
    /// `shutdown()` 照样返回 `Ok(())` —— 结论是对的，坏的是协调任务。故必须查任务句柄。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_coordinator_never_polls_completed_join_future_twice() {
        const ROUNDS: usize = 16;
        let mut panicked = 0usize;
        for round in 0..ROUNDS {
            let (_temp, writer, _generation, _core, _actor) = writer_with_hook(config(), None);
            let res = writer.shutdown().await;
            assert_eq!(res, Ok(()), "round {round}: 干净关闭必须判定为 Ok");
            let Some(coord) = writer.take_coordination_task_for_test().await else {
                panic!("round {round}: 协调任务句柄必须存在（shutdown() 已派生它）");
            };
            match coord.await {
                Ok(()) => {}
                Err(err) if err.is_panic() => panicked += 1,
                Err(err) => panic!("round {round}: 协调任务非 panic 地结束：{err}"),
            }
        }
        assert_eq!(
            panicked, 0,
            "协调任务出现 {panicked}/{ROUNDS} 次 panic ⇒ 对已完成 join future 的二次 poll"
        );
    }

    /// actor 在关闭过程中被取消时，关闭必须判定为 `FailedStop` 且不挂起。
    ///
    /// 注意定位：这条是**覆盖**，不是回归基线。实测 12 轮全部拿到正确结果，
    /// 没有复现关闭协调任务的二次 poll panic——因为生产跑的是 `#[cfg(not(test))]` 变体
    /// （`send_fut` 的 hook 包装与 ack 分支写法不同），单测驱动不到那条路径。
    /// 消除 cfg 分叉才是能写出回归基线的前置条件。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actor_cancelled_during_shutdown_reports_failed_stop_without_hanging() {
        // 每轮都要一个"停在 BeginShutdown 发送之前"的协调任务，好让 actor 先结束。
        for round in 0..12 {
            let (started_tx, started_rx) = oneshot::channel();
            let (resume_tx, resume_rx) = oneshot::channel();
            let hook = ShutdownTestHook::new(started_tx, resume_rx);
            let (_temp, writer, _generation, _core, actor) = writer_with_hook(config(), Some(hook));

            // 顺序要紧：`started_tx` 只有 shutdown() 派生出的协调任务才会消费，
            // 所以必须先发起 shutdown，再等"停在发送之前"的信号。
            let w_shutdown = writer.clone();
            let shutdown_task = tokio::spawn(async move { w_shutdown.shutdown().await });
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .expect("coordinator parked before send")
                .expect("started signal");

            // actor 结束：join future 变 Ready；随后释放暂停 ⇒ 协调任务的 select! 里
            // "发送"与"join 结果"两支同时 ready。
            //
            // 必须等 external task 真正终止再释放：`abort()` 是异步的，取消级联
            // （external future 被删 ⇒ abort_guard 真正 abort 内部 actor、
            // ext_drop 触发 supervisor 收尾）要到 join 返回才完成。少等这一拍时
            // 还活着的内部 actor 会把 BeginShutdown 处理成干净关闭 ⇒ `Ok(())`
            // 误判（实测命中）。
            actor.abort();
            tokio::time::timeout(Duration::from_secs(2), actor)
                .await
                .expect("aborted external task must terminate")
                .expect_err("aborted external task must not finish normally");

            // abort() 是异步的：external task join 成功只保证 abort 级联已启动，
            // 不保证 supervisor 已完成 lifecycle=FAILED_STOP 的状态写入。
            // 等 lifecycle 到达终态再释放 coordinator，否则 coordinator 的 select!
            // 里 send 和 join 都 ready 时 send 可能赢，BeginShutdown 入 channel buffer，
            // 如果 inner actor 还没完全死就可能处理掉 → lifecycle=CLOSED → Ok(()) 误判。
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let state = writer.lifecycle_state();
                    if state == FAILED_STOP || state == CLOSED {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("lifecycle must reach terminal state after actor abort");

            resume_tx
                .send(())
                .expect("release coordinator before-send pause");

            let res = tokio::time::timeout(Duration::from_secs(2), shutdown_task)
                .await
                .unwrap_or_else(|_| panic!("round {round}: 关闭不得挂起"))
                .expect("shutdown task join");
            assert_eq!(
                res,
                Err(WriterError::FailedStop),
                "round {round}: actor 在关闭中被取消必须判定为 FailedStop"
            );
        }
    }

    #[tokio::test]
    async fn business_rejection_and_replay_add_copy_from_missing_source() {
        let initial = config();
        let initial_config = serde_json::to_value(&initial).expect("serialize initial config");
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            initial.clone(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );
        let initial_bytes = std::fs::read(&path).expect("read initial disk config");

        let key = ClientOperationKey(101);
        let mutation = crate::model::ConfigMutation::Add {
            model_id: "copy_target".to_string(),
            context_window: None,
            max_output_tokens: None,
            copy_from: Some("missing_source".to_string()),
            routes: None,
        };

        let res1 = writer.mutate(key, mutation.clone()).await;
        assert!(
            matches!(&res1, Err(WriterError::MutationRejected(err)) if err.contains("unknown source model \"missing_source\""))
        );

        // Zero TestPersistence replacements
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }

        // Zero generation publication
        let snapshot1 = generation.load();
        assert_eq!(snapshot1.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot1.config()).expect("serialize generation"),
            initial_config
        );
        assert!(!snapshot1.config().models.contains_key("copy_target"));

        // Zero core publication
        let core_guard1 = core.lock().await;
        assert_eq!(
            serde_json::to_value(core_guard1.config()).expect("serialize core"),
            initial_config
        );
        assert!(!core_guard1.config().models.contains_key("copy_target"));
        drop(core_guard1);

        // Unchanged revision and disk
        let disk_bytes1 = std::fs::read(&path).expect("read disk config");
        assert_eq!(disk_bytes1, initial_bytes);
        let disk_cfg1 = Config::load(&path).expect("load disk config");
        assert!(!disk_cfg1.models.contains_key("copy_target"));

        // Receipt check
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 99,
                sequence: 1,
            },
        };
        let terminal = writer.lookup(receipt).await.expect("lookup receipt");
        assert_eq!(
            terminal,
            TerminalResponse::Error(res1.as_ref().unwrap_err().clone())
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        assert_eq!(
            phase_hook.phase_count(receipt.id.sequence, OperationPhase::Admitted),
            1
        );

        // Replay before repairing the precondition leaves the entire initial state untouched.
        let replay_before_repair = writer.mutate(key, mutation.clone()).await;
        assert_eq!(replay_before_repair, res1);
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        let snapshot_after_replay = generation.load();
        assert_eq!(snapshot_after_replay.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot_after_replay.config()).expect("serialize generation"),
            initial_config
        );
        {
            let core_guard = core.lock().await;
            assert_eq!(
                serde_json::to_value(core_guard.config()).expect("serialize core"),
                initial_config
            );
        }
        assert_eq!(
            std::fs::read(&path).expect("read disk after initial replay"),
            initial_bytes
        );
        assert_eq!(
            writer
                .lookup(receipt)
                .await
                .expect("lookup after initial replay"),
            terminal
        );

        // Restore external precondition: add "missing_source" via a separate operation
        let add_source = writer
            .mutate(ClientOperationKey(102), add("missing_source"))
            .await;
        assert!(add_source.is_ok());
        let (fresh_receipt, _, _) = add_source.expect("fresh add succeeds");
        assert_eq!(fresh_receipt.id.sequence, receipt.id.sequence + 1);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        assert_eq!(
            phase_hook.phase_count(fresh_receipt.id.sequence, OperationPhase::Admitted),
            1
        );
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 1);
        assert!(
            generation
                .load()
                .config()
                .models
                .contains_key("missing_source")
        );
        let disk_bytes_after_source = std::fs::read(&path).expect("read disk after adding source");

        // Retry exact same operation key and payload
        let res2 = writer.mutate(key, mutation).await;
        assert_eq!(res1, res2);
        assert!(
            matches!(&res2, Err(WriterError::MutationRejected(err)) if err.contains("unknown source model \"missing_source\""))
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(
                phase_hook.phase_count(receipt.id.sequence, phase),
                0,
                "{phase:?}"
            );
            assert_eq!(phase_hook.phase_count(0, phase), 1, "{phase:?}");
        }

        // Zero new TestPersistence replacements, zero new publication, unchanged revision and disk
        assert_eq!(persistence.call_count(), 1);
        let snapshot2 = generation.load();
        assert_eq!(snapshot2.revision(), 1);
        assert!(!snapshot2.config().models.contains_key("copy_target"));

        let core_guard2 = core.lock().await;
        assert!(!core_guard2.config().models.contains_key("copy_target"));
        drop(core_guard2);

        let disk_bytes2 = std::fs::read(&path).expect("read disk after replay");
        assert_eq!(disk_bytes2, disk_bytes_after_source);
        let disk_cfg2 = Config::load(&path).expect("load disk config after replay");
        assert!(!disk_cfg2.models.contains_key("copy_target"));

        // Receipt lookup returns unchanged terminal error
        let terminal_after = writer
            .lookup(receipt)
            .await
            .expect("lookup receipt after replay");
        assert_eq!(terminal_after, terminal);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn business_rejection_and_replay_add_already_existing_model() {
        let mut initial = config();
        initial.models.insert(
            "existing_model".to_string(),
            toml::from_str("context_window = 1024\nmax_output_tokens = 128\n")
                .expect("model config"),
        );
        let initial_config = serde_json::to_value(&initial).expect("serialize initial config");
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            initial.clone(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );
        let initial_bytes = std::fs::read(&path).expect("read initial disk config");

        let key = ClientOperationKey(201);
        let mutation = add("existing_model");
        let res1 = writer.mutate(key, mutation.clone()).await;
        assert!(
            matches!(&res1, Err(WriterError::MutationRejected(err)) if err.contains("model \"existing_model\" already exists"))
        );

        // Zero TestPersistence replacements
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }

        // Zero generation publication
        let snapshot1 = generation.load();
        assert_eq!(snapshot1.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot1.config()).expect("serialize generation"),
            initial_config
        );
        assert_eq!(snapshot1.config().models.len(), 1);

        // Zero core publication
        let core_guard1 = core.lock().await;
        assert_eq!(
            serde_json::to_value(core_guard1.config()).expect("serialize core"),
            initial_config
        );
        assert_eq!(core_guard1.config().models.len(), 1);
        drop(core_guard1);

        // Unchanged revision and disk
        let disk_bytes1 = std::fs::read(&path).expect("read disk config");
        assert_eq!(disk_bytes1, initial_bytes);

        // Receipt check
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 99,
                sequence: 1,
            },
        };
        let terminal = writer.lookup(receipt).await.expect("lookup receipt");
        assert_eq!(
            terminal,
            TerminalResponse::Error(res1.as_ref().unwrap_err().clone())
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        assert_eq!(
            phase_hook.phase_count(receipt.id.sequence, OperationPhase::Admitted),
            1
        );

        // Replay before repairing the precondition leaves the entire initial state untouched.
        let replay_before_repair = writer.mutate(key, mutation.clone()).await;
        assert_eq!(replay_before_repair, res1);
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        let snapshot_after_replay = generation.load();
        assert_eq!(snapshot_after_replay.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot_after_replay.config()).expect("serialize generation"),
            initial_config
        );
        {
            let core_guard = core.lock().await;
            assert_eq!(
                serde_json::to_value(core_guard.config()).expect("serialize core"),
                initial_config
            );
        }
        assert_eq!(
            std::fs::read(&path).expect("read disk after initial replay"),
            initial_bytes
        );
        assert_eq!(
            writer
                .lookup(receipt)
                .await
                .expect("lookup after initial replay"),
            terminal
        );

        // Restore external precondition: remove "existing_model" via another operation
        let remove_res = writer
            .mutate(
                ClientOperationKey(202),
                crate::model::ConfigMutation::Remove {
                    model_id: "existing_model".to_string(),
                    force: true,
                },
            )
            .await;
        assert!(remove_res.is_ok());
        let (fresh_receipt, _, _) = remove_res.expect("fresh remove succeeds");
        assert_eq!(fresh_receipt.id.sequence, receipt.id.sequence + 1);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        assert_eq!(
            phase_hook.phase_count(fresh_receipt.id.sequence, OperationPhase::Admitted),
            1
        );
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 1);
        assert!(
            !generation
                .load()
                .config()
                .models
                .contains_key("existing_model")
        );
        let disk_bytes_after_remove = std::fs::read(&path).expect("read disk after remove");

        // Retry exact same operation key and payload
        let res2 = writer.mutate(key, mutation).await;
        assert_eq!(res1, res2);
        assert!(
            matches!(&res2, Err(WriterError::MutationRejected(err)) if err.contains("model \"existing_model\" already exists"))
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(
                phase_hook.phase_count(receipt.id.sequence, phase),
                0,
                "{phase:?}"
            );
            assert_eq!(phase_hook.phase_count(0, phase), 1, "{phase:?}");
        }

        // Zero new TestPersistence replacements, zero new publication, unchanged revision and disk
        assert_eq!(persistence.call_count(), 1);
        let snapshot2 = generation.load();
        assert_eq!(snapshot2.revision(), 1);
        assert!(!snapshot2.config().models.contains_key("existing_model"));

        let core_guard2 = core.lock().await;
        assert!(!core_guard2.config().models.contains_key("existing_model"));
        drop(core_guard2);

        let disk_bytes2 = std::fs::read(&path).expect("read disk after replay");
        assert_eq!(disk_bytes2, disk_bytes_after_remove);
        let disk_cfg2 = Config::load(&path).expect("load disk config after replay");
        assert!(!disk_cfg2.models.contains_key("existing_model"));

        // Receipt lookup returns unchanged terminal error
        let terminal_after = writer
            .lookup(receipt)
            .await
            .expect("lookup receipt after replay");
        assert_eq!(terminal_after, terminal);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn business_rejection_and_replay_invalid_provider_binding() {
        let mut initial = config();
        initial.models.insert(
            "bound_model".to_string(),
            toml::from_str("context_window = 1024\nmax_output_tokens = 128\n")
                .expect("model config"),
        );
        let initial_config = serde_json::to_value(&initial).expect("serialize initial config");
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) = writer_with_controls(
            initial.clone(),
            Some(phase_hook.clone()),
            Some(persistence.clone()),
        );
        let initial_bytes = std::fs::read(&path).expect("read initial disk config");

        let key = ClientOperationKey(301);
        let mutation = crate::model::ConfigMutation::ProviderAdd {
            model_id: "bound_model".to_string(),
            protocol: crate::config::Protocol::OpenaiChatCompletions,
            provider_id: "missing_provider".to_string(),
            upstream_model: Some("gpt-4".to_string()),
        };

        let res1 = writer.mutate(key, mutation.clone()).await;
        assert!(
            matches!(&res1, Err(WriterError::MutationRejected(err)) if err.contains("unknown provider \"missing_provider\""))
        );

        // Zero TestPersistence replacements
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }

        // Zero generation publication
        let snapshot1 = generation.load();
        assert_eq!(snapshot1.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot1.config()).expect("serialize generation"),
            initial_config
        );
        assert!(
            snapshot1.config().models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );

        // Zero core publication
        let core_guard1 = core.lock().await;
        assert_eq!(
            serde_json::to_value(core_guard1.config()).expect("serialize core"),
            initial_config
        );
        assert!(
            core_guard1.config().models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );
        drop(core_guard1);

        // Unchanged revision and disk
        let disk_bytes1 = std::fs::read(&path).expect("read disk config");
        assert_eq!(disk_bytes1, initial_bytes);
        let disk_cfg1 = Config::load(&path).expect("load disk config");
        assert!(
            disk_cfg1.models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );

        // Receipt check
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 99,
                sequence: 1,
            },
        };
        let terminal = writer.lookup(receipt).await.expect("lookup receipt");
        assert_eq!(
            terminal,
            TerminalResponse::Error(res1.as_ref().unwrap_err().clone())
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        assert_eq!(
            phase_hook.phase_count(receipt.id.sequence, OperationPhase::Admitted),
            1
        );

        // Replay before repairing the precondition leaves the entire initial state untouched.
        let replay_before_repair = writer.mutate(key, mutation.clone()).await;
        assert_eq!(replay_before_repair, res1);
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        let snapshot_after_replay = generation.load();
        assert_eq!(snapshot_after_replay.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot_after_replay.config()).expect("serialize generation"),
            initial_config
        );
        {
            let core_guard = core.lock().await;
            assert_eq!(
                serde_json::to_value(core_guard.config()).expect("serialize core"),
                initial_config
            );
        }
        assert_eq!(
            std::fs::read(&path).expect("read disk after initial replay"),
            initial_bytes
        );
        assert_eq!(
            writer
                .lookup(receipt)
                .await
                .expect("lookup after initial replay"),
            terminal
        );

        // Restore external precondition: add "missing_provider" to config using Commit API
        let mut updated = initial;
        updated.providers.insert(
            "missing_provider".to_string(),
            toml::from_str("api_key_env = \"TEST_KEY\"\nopenai_chat = { url = \"https://example.test/chat\" }\n")
                .expect("provider config"),
        );
        let commit_res = writer.commit(ClientOperationKey(302), updated, 0).await;
        assert!(commit_res.is_ok());
        let (fresh_receipt, _) = commit_res.expect("fresh commit succeeds");
        assert_eq!(fresh_receipt.id.sequence, receipt.id.sequence + 1);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        assert_eq!(
            phase_hook.phase_count(fresh_receipt.id.sequence, OperationPhase::Admitted),
            1
        );
        assert_eq!(persistence.call_count(), 1);
        assert_eq!(generation.load().revision(), 1);
        assert!(
            generation
                .load()
                .config()
                .providers
                .contains_key("missing_provider")
        );
        let disk_bytes_after_commit = std::fs::read(&path).expect("read disk after commit");

        // Retry exact same operation key and payload
        let res2 = writer.mutate(key, mutation).await;
        assert_eq!(res1, res2);
        assert!(
            matches!(&res2, Err(WriterError::MutationRejected(err)) if err.contains("unknown provider \"missing_provider\""))
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(
                phase_hook.phase_count(receipt.id.sequence, phase),
                0,
                "{phase:?}"
            );
            assert_eq!(phase_hook.phase_count(0, phase), 1, "{phase:?}");
        }

        // Zero new TestPersistence replacements, zero new publication, unchanged revision and disk
        assert_eq!(persistence.call_count(), 1);
        let snapshot2 = generation.load();
        assert_eq!(snapshot2.revision(), 1);
        assert!(
            snapshot2.config().models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );

        let core_guard2 = core.lock().await;
        assert!(
            core_guard2.config().models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );
        drop(core_guard2);

        let disk_bytes2 = std::fs::read(&path).expect("read disk after replay");
        assert_eq!(disk_bytes2, disk_bytes_after_commit);
        let disk_cfg2 = Config::load(&path).expect("load disk config after replay");
        assert!(
            disk_cfg2.models["bound_model"]
                .openai_chat_providers
                .is_empty()
        );

        // Receipt lookup returns unchanged terminal error
        let terminal_after = writer
            .lookup(receipt)
            .await
            .expect("lookup receipt after replay");
        assert_eq!(terminal_after, terminal);

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    #[tokio::test]
    async fn reload_load_failure_same_key_replays_terminal_error() {
        let initial = config();
        let initial_config = serde_json::to_value(&initial).expect("serialize initial config");
        let phase_hook = PhaseHook::new();
        let persistence = TestPersistence::new();
        let (_temp, writer, generation, core, task, path) =
            writer_with_controls(initial, Some(phase_hook.clone()), Some(persistence.clone()));
        let initial_generation = generation.load();
        let initial_bytes = std::fs::read(&path).expect("read initial disk config");
        assert_eq!(persistence.load_call_count(), 0);

        // Induce reload load failure by corrupting the configuration file on disk
        std::fs::write(&path, b"invalid toml [[[ syntax corrupt").expect("write corrupt config");
        let invalid_bytes = std::fs::read(&path).expect("read original invalid disk config");

        let key = ClientOperationKey(401);
        let res1 = writer.reload(key).await;
        assert!(matches!(&res1, Err(WriterError::ReloadFailed(err)) if !err.is_empty()));

        // The terminal ack follows one actual load attempt and no replacements or publications.
        assert_eq!(persistence.load_call_count(), 1);
        assert_eq!(persistence.call_count(), 0);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }

        // The original generation and its complete configuration remain unchanged.
        let snapshot1 = generation.load();
        assert!(Arc::ptr_eq(&snapshot1, &initial_generation));
        assert_eq!(snapshot1.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot1.config()).expect("serialize generation"),
            initial_config
        );

        // Zero core publication
        let core_guard1 = core.lock().await;
        assert_eq!(
            serde_json::to_value(core_guard1.config()).expect("serialize core"),
            initial_config
        );
        drop(core_guard1);
        assert_eq!(
            std::fs::read(&path).expect("read disk after failed reload"),
            invalid_bytes
        );

        // Receipt check
        let receipt = OperationReceipt {
            key,
            id: OperationId {
                boot_epoch: 99,
                sequence: 1,
            },
        };
        let terminal = writer.lookup(receipt).await.expect("lookup receipt");
        assert_eq!(
            terminal,
            TerminalResponse::Error(res1.as_ref().unwrap_err().clone())
        );
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        assert_eq!(
            phase_hook.phase_count(receipt.id.sequence, OperationPhase::Admitted),
            1
        );

        // Restore external precondition: write valid config back to disk
        std::fs::write(&path, &initial_bytes).expect("restore valid config on disk");
        assert!(Config::load(&path).is_ok(), "disk config is valid now");
        let restored_bytes = std::fs::read(&path).expect("read restored disk config");
        assert_eq!(restored_bytes, initial_bytes);

        // Retry exact same operation key and payload
        let res2 = writer.reload(key).await;
        assert_eq!(res1.as_ref().unwrap_err(), res2.as_ref().unwrap_err());
        assert!(matches!(&res2, Err(WriterError::ReloadFailed(err)) if !err.is_empty()));

        // Replay returns the cached failure without another load, admission or publication.
        assert_eq!(persistence.load_call_count(), 1);
        assert_eq!(persistence.call_count(), 0);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 1);
        assert_eq!(
            phase_hook.phase_count(receipt.id.sequence, OperationPhase::Admitted),
            1
        );
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 0, "{phase:?}");
        }
        let snapshot2 = generation.load();
        assert!(Arc::ptr_eq(&snapshot2, &initial_generation));
        assert_eq!(snapshot2.revision(), 0);
        assert_eq!(
            serde_json::to_value(snapshot2.config()).expect("serialize generation after replay"),
            initial_config
        );
        let core_guard2 = core.lock().await;
        assert_eq!(
            serde_json::to_value(core_guard2.config()).expect("serialize core"),
            initial_config
        );
        drop(core_guard2);
        assert_eq!(
            std::fs::read(&path).expect("read disk after replay"),
            restored_bytes
        );

        // Receipt lookup returns unchanged terminal error
        let terminal_after = writer
            .lookup(receipt)
            .await
            .expect("lookup receipt after replay");
        assert_eq!(terminal_after, terminal);

        // A fresh reload really loads the restored file; replay consumed no sequence.
        let (fresh_receipt, _) = writer
            .reload(ClientOperationKey(402))
            .await
            .expect("fresh reload succeeds");
        assert_eq!(fresh_receipt.id.sequence, receipt.id.sequence + 1);
        assert_eq!(phase_hook.phase_count(0, OperationPhase::Admitted), 2);
        assert_eq!(
            phase_hook.phase_count(fresh_receipt.id.sequence, OperationPhase::Admitted),
            1
        );
        assert_eq!(persistence.load_call_count(), 2);
        assert_eq!(persistence.call_count(), 0);
        assert_eq!(generation.load().revision(), 1);
        for phase in [
            OperationPhase::BeforeCorePublish,
            OperationPhase::BeforeGenerationPublish,
            OperationPhase::Published,
        ] {
            assert_eq!(phase_hook.phase_count(0, phase), 1, "{phase:?}");
        }
        assert_eq!(
            std::fs::read(&path).expect("read disk after fresh reload"),
            restored_bytes
        );

        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }

    /// : `send_and_wait` 必须在预算内返回 `AckTimeout`，而不是永久挂起。
    ///
    /// 场景：writer 收了 Commit 消息（send 成功）但在 `Admitted` 阶段被暂停，
    /// 永远不会发出 ack。`send_and_wait` 必须在预算到期后返回 `AckTimeout`。
    #[tokio::test]
    async fn send_and_wait_returns_ack_timeout_when_writer_does_not_respond() {
        let phase_hook = PhaseHook::new();
        let (_temp, writer, _generation, _core, task, _path) =
            writer_with_controls(config(), Some(phase_hook.clone()), None);

        // 把 ack 预算改到 50ms——足够短让测试快速通过，又足够长避免假阳性。
        writer.set_commit_ack_timeout_for_test(Duration::from_millis(50));

        // 暂停 writer 在 Admitted 阶段：消息已被接收并进入处理，但不会继续到发 ack。
        let mut pause = phase_hook.pause(1, OperationPhase::Admitted);

        let writer_for_commit = writer.clone();
        let commit_task = tokio::spawn(async move {
            writer_for_commit
                .commit(ClientOperationKey(42), config(), 0)
                .await
        });

        // 等 writer 到达 Admitted 阶段——确认消息已被接收，ack 不会发出。
        pause.wait_reached().await;

        // commit 必须在合理时间内返回 AckTimeout（50ms 预算 + 一些调度余量）。
        let result = tokio::time::timeout(Duration::from_secs(2), commit_task)
            .await
            .expect("commit task itself must not hang")
            .expect("join");

        assert_eq!(result, Err(WriterError::AckTimeout));

        // 释放 writer，让它完成处理并正常关闭。
        pause.release();
        writer.shutdown().await.expect("shutdown");
        task.await.expect("join");
    }
}
