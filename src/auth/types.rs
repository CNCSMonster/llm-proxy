use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use super::openai::schema::OpenaiAccount;

// ============================================================================
// 新 OAuth 账号存储结构（按类型分组）
// ============================================================================

/// 顶层 OAuth 账号存储结构。
///
/// - 类型化字段 = **唯一活跃存储**（`default` + `skip_serializing_if`）；
/// - `others`（`#[serde(flatten)]`）只承载遗留/未知顶层键（历史残留、
///   未来版本字段），**序列化原样展开、取数路径一律不读**（peek/get 对其不可见，
///   查不到即 NotFound）；字段序 = 声明序，`others` 键追加于末尾。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountsFile {
    pub version: u32,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub openai: HashMap<String, OpenaiAccount>,
    /// 遗留/未知顶层键保全（flatten：已知名键绝不落入本字段）。
    #[serde(default, flatten)]
    pub others: BTreeMap<String, Value>,
}

/// 暂存的未持久化 OAuth 凭据（候选状态，参与内存验证并在成功时提交）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingAccount {
    pub auth_type: String,
    pub account: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl PendingAccount {
    pub fn to_resolved_auth(&self) -> crate::config::ResolvedAuth {
        crate::config::ResolvedAuth {
            token: Some(self.access_token.clone()),
            project_id: self.project_id.clone(),
        }
    }
}

impl AccountsFile {
    pub fn new() -> Self {
        Self {
            version: 1,
            openai: HashMap::new(),
            others: BTreeMap::new(),
        }
    }
}

/// 被跳过的无效账号信息
#[derive(Debug, Clone)]
pub struct SkippedAccount {
    pub account_type: String, // "openai"
    pub account_id: String,
    pub reason: String,
}

pub(super) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
