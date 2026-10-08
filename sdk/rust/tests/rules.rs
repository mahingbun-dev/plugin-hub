//! **跨语言规则的契约测试**——读的是 Go 侧那份 `hub-rules.json`。
//!
//! 事实来源只有一份：`sdk/go/hubkit/testdata/hub-rules.json`。中台的 Rust
//! （`crates/hub-grpc/tests/state_rules.rs`）、Go SDK、以及本 SDK 都拿它当断言清单。
//! 于是「某一侧改了实现而没同步契约」会让**那一侧**变红，而不是留到线上表现为
//! 「插件以为写进去了、中台拒绝」。
//!
//! 用例是**手写**的规格声明，不由任何一侧的实现生成——由实现生成等于自证。

use std::path::PathBuf;

use serde_json::Value;

/// `sdk/rust/` 下相对仓库的 rules 文件位置。
fn rules_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../go/hubkit/testdata/hub-rules.json")
}

#[test]
fn 两侧规则实现与同一份契约文件相符() {
    let path = rules_path();

    // SDK 是**随包发出去**的（生成出来的工程里它在 <工程>/sdk/ 下），
    // 那里旁边没有 Go SDK。这不是缺陷，跳过即可——但不能静静跳过：
    // 一旦哪天有人把 Go SDK 挪走，这条测试会从「在验」变成「没在验」而没人发现。
    if !path.exists() {
        let go_sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../go");
        assert!(
            !go_sdk.exists(),
            "Go SDK 在 {} 却找不到规则文件 {} —— 它被挪走或改名了，\
             而这条测试正是靠它才在验跨语言一致性",
            go_sdk.display(),
            path.display()
        );
        eprintln!(
            "跳过跨语言规则测试：{} 不存在（本 SDK 不在中台仓库检出里，属正常）",
            path.display()
        );
        return;
    }

    let raw = std::fs::read_to_string(&path).expect("规则文件应当可读");
    let doc: Value = serde_json::from_str(&raw).expect("规则文件应当是合法 JSON");
    let cases = doc["cases"].as_array().expect("规则文件应当有 cases 数组");
    assert!(!cases.is_empty(), "用例清单不该是空的");

    let mut checked = 0;
    for case in cases {
        let rule = case["rule"].as_str().expect("每个用例都要有 rule");
        let input = case["input"].as_str().expect("每个用例都要有 input");
        let expected = case["valid"].as_bool().expect("每个用例都要有 valid");

        let actual = match rule {
            "stateSegment" => hubkit::rules::valid_state_segment(input),
            "pluginName" => hubkit::rules::valid_plugin_name(input),
            other => panic!("契约文件里有本 SDK 不认识的规则 {other:?}"),
        };

        assert_eq!(
            actual, expected,
            "规则 {rule} 判定 {input:?} 得到 {actual}，契约要求 {expected}"
        );
        checked += 1;
    }

    assert_eq!(checked, cases.len());
    eprintln!("跨语言规则一致性：{checked} 条用例全过");
}

#[test]
fn 状态凭证的_metadata_键名与契约一致() {
    // 键名写在契约文件里，两侧都要照它取——写错的表现是「凭证取不到」，
    // 而那个错误在中台侧只是一句「未认证」，看不出是拼写问题。
    //
    // 比对的是 **SDK 里那个常量**（Go 侧 `rules_test.go` 做的也是这件事）：
    // 只断言字面量的话，常量改成别的字符串时这条测试照样绿。
    let path = rules_path();
    if !path.exists() {
        return;
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let from_contract = doc["stateTokenMetadata"]
        .as_str()
        .expect("契约文件应当有 stateTokenMetadata");
    assert_eq!(
        hubkit::state::STATE_TOKEN_METADATA,
        from_contract,
        "状态凭证的 metadata 键名与契约文件不一致"
    );
    // 契约文件自己也不该被人顺手改掉
    assert_eq!(from_contract, "x-hub-state-token");
}

#[test]
fn 互调链的_meta_键名与契约一致() {
    // 与 stateTokenMetadata 同一条纪律：互调链经 `meta[hub.call_chain]` 传递，
    // SDK 写错键名时中台读不到链，防环与链深限制**静默失效**——直到某天互调成环
    // 打爆下游才暴露。事实源在契约文件 `gateway.callChainMeta`，SDK 的常量照它对齐。
    let path = rules_path();
    if !path.exists() {
        return;
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let gateway = doc
        .get("gateway")
        .expect("契约文件应当有 gateway 小节（插件互调的事实源）");
    let from_contract = gateway["callChainMeta"]
        .as_str()
        .expect("gateway 小节应当有 callChainMeta");
    assert_eq!(
        hubkit::gateway::CALL_CHAIN_META,
        from_contract,
        "互调链的 meta 键名与契约文件不一致"
    );
    // 契约文件自己也不该被人顺手改掉
    assert_eq!(from_contract, "hub.call_chain");
}

#[test]
fn 状态上限与中台实现一致() {
    // 单值上限与扫描上限的**定义处**在中台，SDK 里那两个常量是它的副本，
    // 为的是能在本地就给出好懂的错。副本没有守卫就会悄悄漂移，而漂移的表现是
    // 「本地放行、中台拒绝」——比两边都拒更难查。这条测试把两份钉在一起。
    //
    // 不写死某个文件路径：这两个常量在中台侧搬过一次家（`hub-grpc` → `hub-core`，
    // 因为本地替身 mock 中台要用同一份判据却拖不动 tonic 服务端），以后还可能再搬。
    // 所以在 `crates/` 下扫一遍源码，只要**有一处**定义符合预期就算过。
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates");

    // 与 rules_path 同一个道理：SDK 随包发出去时旁边没有中台仓库，跳过即可——
    // 但不能静静跳过，否则哪天中台源码挪走了，这条会从「在验」变成「没在验」。
    if !crates_dir.is_dir() {
        eprintln!(
            "跳过状态上限一致性测试：{} 不存在（本 SDK 不在中台仓库检出里，属正常）",
            crates_dir.display()
        );
        return;
    }

    // 去掉空白再比对：只容忍排版变化，不容忍数值变化
    let expect_value = "MAX_VALUE_BYTES:usize=1024*1024";
    let expect_scan = "MAX_SCAN_LIMIT:u32=1000";

    let mut saw_value = Vec::new();
    let mut saw_scan = Vec::new();
    let mut visited = 0;
    for file in rust_files(&crates_dir) {
        visited += 1;
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        let flat: String = source.chars().filter(|c| !c.is_whitespace()).collect();
        if flat.contains(expect_value) || flat.contains(expect_scan) {
            if flat.contains(expect_value) {
                saw_value.push(file.display().to_string());
            }
            if flat.contains(expect_scan) {
                saw_scan.push(file.display().to_string());
            }
        }
    }
    assert!(visited > 0, "在中台源码里一个 .rs 都没扫到，扫描逻辑坏了");

    assert!(
        !saw_value.is_empty(),
        "中台侧已经找不到 `MAX_VALUE_BYTES: usize = 1024 * 1024` 了——\
         要么是上限改了（请同步 sdk/rust/src/state.rs，那里现在声明的是 {}），\
         要么是它又搬了家（把新落点加进本测试的扫描范围）",
        hubkit::MAX_VALUE_BYTES
    );
    assert!(
        !saw_scan.is_empty(),
        "中台侧已经找不到 `MAX_SCAN_LIMIT: u32 = 1000` 了——\
         要么是上限改了（请同步 sdk/rust/src/state.rs，那里现在声明的是 {}），\
         要么是它又搬了家",
        hubkit::MAX_SCAN_LIMIT
    );
    eprintln!(
        "状态上限一致性：定义处 {} / {}",
        saw_value.join(", "),
        saw_scan.join(", ")
    );

    // 两侧数值一致（上面钉住了中台那份，这里钉住 SDK 这份）
    assert_eq!(hubkit::MAX_VALUE_BYTES, 1024 * 1024);
    assert_eq!(hubkit::MAX_SCAN_LIMIT, 1000);
}

/// 递归收集 `.rs` 文件，跳过 `target` 与隐藏目录。
///
/// 只扫有界的深度与文件数：这条测试要在任何人的机器上秒过，
/// 不能因为某个 crate 旁边挂了个巨大的构建产物目录就慢下来。
fn rust_files(dir: &std::path::Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 6;
    const MAX_FILES: usize = 5000;

    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_FILES {
                return out;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}
