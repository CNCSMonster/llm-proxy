//! 规范化 byte-equal roundtrip（准入门禁）。
//!
//! **byte-equal = `serde_json::to_string` 规范化输出后逐字节相等**；
//! fixture 四前提（无重复键 / 数字预规范化 i64·u64 整数域 / 无非常规转义 /
//! 无非常规空白）**由构造时断言**。
//!
//! 落点说明：本 crate 为 bin-only
//! （无 lib target，integration tests 无法链接 crate 内部）→ 落为 `auth::storage_roundtrip`
//! 内联模块（与仓库测试风格一致），语义与判据不变。
//!
//! 覆盖两条不变量：
//!   ① 规范化字节稳定：结构体 ⇄ 文件往返后，规范化输出逐字节不变；
//!   ② fixture 四前提成立：手写 fixture 即规范化形式（canonical）。

use super::storage::{load_oauth_accounts, save_oauth_accounts};
use super::types::AccountsFile;

/// 手写规范化 fixture（**compact 单行** = `serde_json::to_string` 的输出形式）：
/// 字段序 = 结构体声明序；数字全为整数；无重复键；无非常规转义/空白。
const FIXTURE_COMPACT: &str = concat!(
    r#"{"version":1,"#,
    r#""openai":{"work":{"account_label":"p0b@example.com","#,
    r#""access_token":"fixture-access-token-0000000002","#,
    r#""refresh_token":"fixture-refresh-token-0000000002","#,
    r#""expires_at_unix":1785000000,"updated_at_unix":1784990000}}}"#
);

/// 四前提构造时断言：fixture 必须已是**结构体路径的规范化形式**——
/// `fixture → AccountsFile → to_string` 逐字节回到原文。
/// serde 结构体反序列化语义天然覆盖四前提：
///   - 无重复键：serde 对重复字段报 `duplicate field` 错；
///   - 数字预规范化 i64·u64 整数域：字段类型即 i64/u32，非整数或越界直接报错；
///   - 无非常规转义 / 无非常规空白：`to_string` 输出紧凑单行、转义归一——
///     任何非常规空白或转义都会使字节失配；
///   - 附带字段序前提：输出按结构体声明序，fixture 必须同序。
fn assert_canonical(fixture: &str) -> AccountsFile {
    let accounts: AccountsFile = serde_json::from_str(fixture).unwrap_or_else(|err| {
        panic!("fixture 反序列化失败（四前提之一被破坏：重复键/数字非整数域/类型漂移）: {err}")
    });
    let reserialized = serde_json::to_string(&accounts).expect("规范化序列化");
    assert_eq!(
        reserialized, fixture,
        "fixture 非规范化形式（四前提之一被破坏：非常规空白/非常规转义/字段序漂移）"
    );
    accounts
}

/// 主断言：fixture 四前提 + 结构体/文件两级规范化 byte-equal。
#[test]
fn storage_roundtrip_is_canonical_byte_equal() {
    // ── 四前提（构造时断言）──
    let accounts = assert_canonical(FIXTURE_COMPACT);
    let before = serde_json::to_string(&accounts).expect("规范化");
    assert_eq!(
        before, FIXTURE_COMPACT,
        "结构体规范化输出与 fixture 不一致（字段序/字段集漂移）"
    );

    // ── 文件级：save → load → save 往返逐字节不变 ──
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("oauth_accounts.json");
    save_oauth_accounts(&path, &accounts).expect("save #1");
    let bytes1 = std::fs::read(&path).expect("read #1");

    let (reloaded, skipped) = load_oauth_accounts(&path).expect("load");
    assert!(skipped.is_empty(), "fixture 不应有被跳过的无效账号");
    let reloaded_canonical = serde_json::to_string(&reloaded).expect("规范化 reloaded");
    assert_eq!(
        reloaded_canonical, before,
        "① 规范化 byte-equal：load 后结构体的 to_string 输出漂移"
    );

    save_oauth_accounts(&path, &reloaded).expect("save #2");
    let bytes2 = std::fs::read(&path).expect("read #2");
    assert_eq!(
        bytes1, bytes2,
        "① 文件级 byte-equal：load→save 往返后文件字节漂移"
    );
}

// ── `others` flatten 兼容策略（存储侧前提）──

/// 未知顶层键随文件往返原样保全：反序列化进 `others`、再序列化逐字节回到原位
/// （fixture 中未知键置于**末尾** = flatten 输出序）。
#[test]
fn others_unknown_top_level_key_survives_roundtrip_byte_equal() {
    let fixture = concat!(
        r#"{"version":1,"#,
        r#""openai":{"work":{"account_label":"p2b@example.com","#,
        r#""access_token":"fixture-access-token-0000000002","#,
        r#""refresh_token":"fixture-refresh-token-0000000002","#,
        r#""expires_at_unix":1785000000,"updated_at_unix":1784990000}},"#,
        r#""future_thing":{"n":3,"nested":[1,"two",true]}}"#
    );
    let loaded: AccountsFile = serde_json::from_str(fixture).expect("反序列化（含未知键）");
    // ① 未知键进 others，不进类型化字段
    assert!(
        loaded.others.contains_key("future_thing"),
        "{:?}",
        loaded.others
    );
    assert!(loaded.others.len() == 1);
    assert!(!loaded.openai.is_empty() && !loaded.others.is_empty());
    // ② 规范化序列化逐字节回到原 fixture（named 字段序 + others 末尾展开）
    let out = serde_json::to_string(&loaded).expect("序列化");
    assert_eq!(out, fixture, "others 往返必须逐字节保真");
}

/// `others` 中的未知分组条目绝不被类型化取数看见（取数只读类型化字段）。
#[test]
fn others_is_invisible_to_typed_access_paths() {
    let fixture = concat!(
        r#"{"version":1,"#,
        r#""future_group":{"evil":{"access_token":"from-others"}}}"#
    );
    let loaded: AccountsFile = serde_json::from_str(fixture).expect("反序列化");
    // 未知键只落 others，不进任何类型化分组
    assert!(loaded.others.contains_key("future_group"));
    assert!(loaded.openai.is_empty());
    // 类型化路径无此账号 → 唯一入口查到即无（get_token 的 NotFound 前提）
    assert!(!loaded.openai.contains_key("evil"));
}
