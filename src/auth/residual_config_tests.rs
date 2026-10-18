//! 残留/未知认证类型的反序列化报错口径。
//!
//! ③ 残留配置报错：已移除的认证类型必须报 unknown variant；输入值全文只出现 1 次
//!    （= 回显用户输入，允许；≥2 次 = 混进支持列表 = 红）；支持列表只列本树公开
//!    类型；message 不含任何内部材料词。
//! ④ 残留存储：含已移除分组键的 JSON 解析为 AccountsFile 不 panic；
//!    `others` 原样保有全部残留键；类型化分组看不到；typed 取数路径 NotFound。

use crate::auth::types::AccountsFile;
use crate::config::ProviderConfig;
use serde_json::json;

/// 未知认证类型 → 常规 unknown variant 文案；支持列表只列本树类型；零内部材料引用。
#[test]
fn unknown_auth_type_error_is_plain_and_leaks_nothing() {
    let err = serde_json::from_value::<ProviderConfig>(json!({
        "auth": {"type": "some_removed_auth_type"}
    }))
    .expect_err("裁剪树上已移除类型必须是未知认证类型");
    let msg = err.to_string();
    // 常规 serde unknown-variant 形态（回显用户输入 = 设计允许）
    assert!(msg.contains("unknown variant"), "got: {msg}");
    // 回显用户输入允许出现 1 次；≥2 次 = 混进了支持列表 = 红
    let occurrences = msg.matches("some_removed_auth_type").count();
    assert_eq!(occurrences, 1, "输入值只允许作为回显出现 1 次：{msg}");
    // 支持列表 = 本树存在的公开类型（serde 由变体集自动生成）
    for supported in ["api_key_env", "openai_oauth", "none"] {
        assert!(msg.contains(supported), "支持列表缺 {supported}：{msg}");
    }
    // 零内部材料引用（迁移指引/私有材料/内部流程标识一律不得进用户可见文案）
    for forbidden in [
        "migration",
        "private",
        "internal",
        concat!("issue-", "233"),
        "llm-proxy-docs",
        "清退",
        "迁移",
    ] {
        assert!(
            !msg.contains(forbidden),
            "文案泄漏内部材料 {forbidden:?}：{msg}"
        );
    }
}

/// ④ 残留存储保全 + typed 取数 NotFound。
#[test]
fn residual_storage_preserved_in_others() {
    let json_str = r#"{
        "version": 1,
        "some_removed_group": {
            "test-account": {
                "access_token": "tok_test_0000000000",
                "refresh_token": "ref_test_0000000000",
                "project_id": "proj_test",
                "expires_at_unix": 9999999999,
                "email": "test@example.com"
            }
        },
        "some_unknown_key": 1
    }"#;

    // 解析不 panic（残留分组键经 flatten 落入 others）
    let accounts: AccountsFile = serde_json::from_str(json_str).expect("parse residual");
    assert_eq!(accounts.version, 1);
    assert!(accounts.openai.is_empty(), "类型化分组看不到残留条目");

    // others 原样保有两键，不丢失
    assert_eq!(
        accounts.others.len(),
        2,
        "others 应原样保有 some_removed_group 与 some_unknown_key：{:?}",
        accounts.others.keys().collect::<Vec<_>>()
    );
    assert!(accounts.others.contains_key("some_removed_group"));
    assert!(accounts.others.contains_key("some_unknown_key"));

    // typed 取数路径 NotFound：把该文件落到磁盘，经 facade 按 openai 配置取数
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("oauth_accounts.json");
    std::fs::write(&path, json_str).expect("write residual file");
    let cfg: ProviderConfig = serde_json::from_value(json!({"auth": {"type": "openai_oauth"}}))
        .expect("openai provider config");
    let err = crate::auth::get_token_valid("prov", &cfg, &path)
        .expect_err("残留存储对 typed 取数必须 NotFound");
    let text = format!("{err:#}");
    assert!(
        text.contains("not found") || text.contains("requires OAuth login"),
        "typed 取数错误应为 NotFound/登录引导：{text}"
    );
}
