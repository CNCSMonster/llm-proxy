//! storage 容器投影事务（`with_transaction`）。
//!
//! `StorageHandle` 只持路径（storage 容器载体——不引入新句柄型依赖，路径即
//! 现有 flock/原子写机制的全部输入）；事务 = `with_locked_accounts`（flock + scopeguard
//! 读改写，并发断言覆盖的既有原语）+ **写回按序列化字节变化判定**（写 iff 变化
//! ——对齐现状「仅刷新/变更时写」语义，无变化不产生备份与 mtime 抖动）。
//!
//! 闭包只见 `AccountGroup<Self>`（聚合容器布局对闭包不可见）；序列化写回
//! 展开即原值——`others` 与兄弟字段「写回绝不触碰」由同一机制保证。

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::storage::{save_oauth_accounts_locked, with_locked_accounts};
use super::traits::{AccountGroup, ProviderAuth};

/// storage 容器载体（路径即句柄；**不出 auth 模块**——可见性 `pub(in crate::auth)`）。
pub(in crate::auth) struct StorageHandle {
    path: PathBuf,
}

impl StorageHandle {
    pub(in crate::auth) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    pub(in crate::auth) fn path(&self) -> &Path {
        &self.path
    }

    /// flock 事务内读改写：`P::group` 投影 → 闭包 → **序列化有变化才写回**
    /// （tmp+rename+目录 fsync+备份裁剪，`save_oauth_accounts_locked` 既有机制）。
    /// 写回前对**变化条目**跑 `P::validate_account`（能力方法消费）——**失败仅
    /// 告警不阻断**：现状写回无校验，从严报错会改变既有行为（零破坏约束）；
    /// 未变动的存量条目不校验（不给存量数据加新约束）。
    pub(in crate::auth) fn with_transaction<P: ProviderAuth, T>(
        &self,
        f: impl FnOnce(&mut AccountGroup<P>) -> Result<T>,
    ) -> Result<T> {
        with_locked_accounts(&self.path, |accounts| {
            let before = serde_json::to_string(accounts)?;
            let group_before = group_snapshot::<P>(accounts)?;
            let out = f(P::group(accounts))?;
            let after = serde_json::to_string(accounts)?;
            if after != before {
                warn_invalid_changed::<P>(&group_before, accounts)?;
                save_oauth_accounts_locked(&self.path, accounts)?;
            }
            Ok(out)
        })
    }
}

/// 变化前的 `P::group` 序列化快照（逐键比较用；`Value` 对象键 = 账号名）。
fn group_snapshot<P: ProviderAuth>(accounts: &mut super::types::AccountsFile) -> Result<Value> {
    Ok(serde_json::to_value(P::group(accounts))?)
}

/// 写回前校验变化条目：新增/修改的键逐个 `P::validate_account`，失败告警不阻断
/// （事件 `oauth_account_validation_warning`，account 经标签脱敏）。`P::KIND` →
/// `auth_kind` 字段（能力方法消费点之一）。
fn warn_invalid_changed<P: ProviderAuth>(
    group_before: &Value,
    accounts: &mut super::types::AccountsFile,
) -> Result<()> {
    let before = group_before.as_object();
    let group = P::group(accounts);
    for (id, entry) in group.iter() {
        let changed = before.and_then(|b| b.get(id)) != Some(&serde_json::to_value(entry)?);
        if changed && let Err(err) = P::validate_account(id, entry) {
            tracing::warn!(
                target: "llm_proxy::oauth",
                event = "oauth_account_validation_warning",
                auth_kind = P::KIND.as_auth_kind_str(),
                account = %crate::auth::refresh::oauth_account_label(id),
                error = %err,
                "changed account entry failed validation (write proceeds)"
            );
        }
    }
    Ok(())
}
