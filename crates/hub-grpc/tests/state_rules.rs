//! 跨语言规则契约测试。
//!
//! 事实来源是 `sdk/go/hubkit/testdata/hub-rules.json`——Go 侧的
//! `sdk/go/hubkit/rules_test.go` 读同一份文件。任一侧改了规则而没改契约文件，
//! 或改了契约文件而某一侧没跟上，这里就会红。
//!
//! 路径是 `include_str!` 相对于本文件所在目录（`crates/hub-grpc/tests/`）算的。
//! 契约文件放 Go 模块内是因为 Go 的 `//go:embed` 要求文件在包目录树内，
//! 而 `include_str!` 可以跨目录——反过来会让 Go 侧退化成脆弱的运行时路径读取。

use hub_grpc::state::{STATE_TOKEN_METADATA, valid_segment};
use hub_registry::is_valid_plugin_name;
use serde_json::Value;

const RULES_JSON: &str = include_str!("../../../sdk/go/hubkit/testdata/hub-rules.json");

/// 每条规则的用例数下限。用下限而不是相等：加用例不必同时改两侧测试文件，
/// 而删掉任何一条都会让计数掉到下界以下，从而判红。
const MIN_STATE_SEGMENT_CASES: usize = 21;
const MIN_PLUGIN_NAME_CASES: usize = 16;

/// 解析契约文件，并挡掉「文件被清空导致测试恒真」这种失效。
fn rules() -> Value {
    let parsed: Value = serde_json::from_str(RULES_JSON).expect("契约文件应是合法 JSON");

    let cases = parsed["cases"].as_array().expect("契约文件应有 cases 数组");
    assert!(
        !cases.is_empty(),
        "契约文件里没有任何用例——测试会恒真，守卫形同虚设"
    );

    // rule 名只允许这两个。敲错的 rule（stateSegement、带空格、大小写不符）会让
    // 两侧的契约测试**同时**跳过该用例——checked 仍 > 0，一片绿，而这条用例从此
    // 不再守护任何东西。所以未知 rule 直接判红（也顺带挡住「加了新规则名却没实现」）。
    for case in cases {
        let rule = case["rule"].as_str().expect("rule 应是字符串");
        assert!(
            rule == "stateSegment" || rule == "pluginName",
            "契约文件里出现未知 rule {rule:?}——名字敲错或新增规则未实现"
        );
    }

    // 只确认这两条规则对象还在——挡的是「文件被截断 / 规则段被删」这种失效，
    // **不**校验 maxBytes / allowedChars 的具体值（那由各自的用例覆盖）。
    for name in ["stateSegment", "pluginName"] {
        assert!(!parsed["rules"][name].is_null(), "契约文件缺少规则 {name}");
    }

    parsed
}

#[test]
fn 状态凭证键名与契约一致() {
    let parsed = rules();
    let want = parsed["stateTokenMetadata"]
        .as_str()
        .expect("契约文件应有 stateTokenMetadata");
    assert_eq!(
        STATE_TOKEN_METADATA, want,
        "两侧的凭证 metadata 键必须一致，否则中台会判成「无凭证」"
    );
}

#[test]
fn 状态键规则符合契约() {
    let parsed = rules();
    let mut checked = 0;

    for case in parsed["cases"].as_array().unwrap() {
        if case["rule"] != "stateSegment" {
            continue;
        }
        let input = case["input"].as_str().expect("input 应是字符串");
        let want = case["valid"].as_bool().expect("valid 应是布尔");
        checked += 1;

        assert_eq!(
            valid_segment(input),
            want,
            "valid_segment({input:?}) 与契约文件不符"
        );
    }

    assert!(
        checked >= MIN_STATE_SEGMENT_CASES,
        "stateSegment 用例数 = {checked}，少于下限 {MIN_STATE_SEGMENT_CASES}——删掉任何一条都会让守卫静默变弱"
    );
}

#[test]
fn 插件名规则符合契约() {
    let parsed = rules();
    let mut checked = 0;

    for case in parsed["cases"].as_array().unwrap() {
        if case["rule"] != "pluginName" {
            continue;
        }
        let input = case["input"].as_str().expect("input 应是字符串");
        let want = case["valid"].as_bool().expect("valid 应是布尔");
        checked += 1;

        assert_eq!(
            is_valid_plugin_name(input),
            want,
            "is_valid_plugin_name({input:?}) 与契约文件不符"
        );
    }

    assert!(
        checked >= MIN_PLUGIN_NAME_CASES,
        "pluginName 用例数 = {checked}，少于下限 {MIN_PLUGIN_NAME_CASES}——删掉任何一条都会让守卫静默变弱"
    );
}

/// 网关常量与契约一致（互调链 meta 键 / 深度上限 / 配额键前缀与额度）。
///
/// SDK 的 invoke 便捷方法要按同一份事实拼链、判断能不能再调、对齐配额语义；
/// 任一侧单独改了（比如把链键换成别的名字），跨语言的行为就会静默分叉——
/// 插件之间调不通，而各自的测试还都绿着。
#[test]
fn 网关规则符合契约() {
    let parsed = rules();
    let gateway = &parsed["gateway"];
    assert!(!gateway.is_null(), "契约文件缺少 gateway 小节");

    assert_eq!(
        hub_grpc::gateway::CALL_CHAIN_META,
        gateway["callChainMeta"]
            .as_str()
            .expect("callChainMeta 应是字符串"),
        "互调链的 meta 键两侧必须一致，否则中台永远检不出环"
    );
    assert_eq!(
        hub_grpc::gateway::MAX_INVOKE_DEPTH,
        gateway["maxInvokeDepth"]
            .as_u64()
            .expect("maxInvokeDepth 应是数字") as usize,
        "深度上限两侧必须一致，否则一侧的合法调用会被另一侧判超限"
    );
    assert_eq!(
        hub_grpc::gateway::INVOKE_QUOTA_KEY_PREFIX,
        gateway["invokeQuotaKeyPrefix"]
            .as_str()
            .expect("invokeQuotaKeyPrefix 应是字符串"),
        "配额键前缀两侧必须一致，否则 SDK 预估的额度与中台记的不是同一本账"
    );
    assert_eq!(
        hub_grpc::gateway::INVOKE_QUOTA_PER_MINUTE,
        gateway["invokeQuotaPerMinute"]
            .as_i64()
            .expect("invokeQuotaPerMinute 应是数字"),
        "每分钟额度两侧必须一致"
    );
}
