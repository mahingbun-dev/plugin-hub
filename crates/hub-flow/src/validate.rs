//! 编排的静态校验。
//!
//! 每条检查都对应一类「不改就会在生产上静默跑偏」的问题，因此刻意都在**保存时**做，
//! 而不是等执行时才暴露：连到不存在的插件、连成环、上下游契约对不上——这些错误
//! 在运行时表现为「某个节点拿不到数据」，排查起来极其费劲。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::definition::{is_valid_name, FlowDefinition, MAX_NODES};

/// 节点超时的建议上限。超过它多半意味着这个插件该改成异步处理。
pub const MAX_NODE_TIMEOUT_MS: i64 = 300_000;

/// 某个插件在中台注册表里的可用情况。
///
/// 校验器据此解析每个节点的版本约束——**版本约束该由校验器检查**，
/// 「没有满足约束的版本」是编排问题，不是调用方的问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAvailability {
    pub plugin: String,

    /// 可用版本，**按新到旧排序**（[`pick_version`] 依赖这个顺序取「最新」）
    pub versions: Vec<PluginVersion>,
}

/// 插件的某一个版本及其契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginVersion {
    pub version: String,

    /// 该版本生产的消息全限定名
    pub produces: Vec<String>,

    /// 该版本消费的消息全限定名
    pub consumes: Vec<String>,
}

/// 节点解析出来的目标：用哪个插件的哪个版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNode {
    pub plugin: String,
    pub version: String,
    pub produces: Vec<String>,
    pub consumes: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Severity {
    /// 拦下：保存会被拒绝
    Error,
    /// 放行但提示
    Warning,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Code {
    InvalidName,
    NoNodes,
    TooManyNodes,
    DuplicateNodeId,
    UnknownNode,
    Cycle,
    FanIn,
    NoEntry,
    IsolatedNode,
    PluginUnresolved,
    ContractMismatch,
    TimeoutTooLarge,
}

/// 一条校验结果。
///
/// 可序列化是有意的：它会被存进 `flow_revisions.validation`，
/// 让排障时能回答「当时为什么存不下去」而不用去翻日志。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct FlowIssue {
    pub severity: Severity,
    pub code: Code,

    /// 涉及的对象：flow 名或节点 id
    pub subject: String,

    /// 人话描述，可直接展示给编排的人
    pub detail: String,
}

impl FlowIssue {
    fn error(code: Code, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    fn warn(code: Code, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// 是否存在会拦下保存的问题。
pub fn has_errors(issues: &[FlowIssue]) -> bool {
    issues.iter().any(FlowIssue::is_error)
}

/// 把错误汇总成一句话。
pub fn summarize(issues: &[FlowIssue]) -> Option<String> {
    let errors: Vec<&FlowIssue> = issues.iter().filter(|i| i.is_error()).collect();
    let first = errors.first()?;

    if errors.len() == 1 {
        Some(first.detail.clone())
    } else {
        Some(format!(
            "{}（另有 {} 处问题）",
            first.detail,
            errors.len() - 1
        ))
    }
}

/// 校验一条编排。结果按严重程度、类型、涉及对象稳定排序。
pub fn validate(
    flow: &FlowDefinition,
    available: &BTreeMap<String, PluginAvailability>,
) -> Vec<FlowIssue> {
    let mut issues = Vec::new();

    check_name(&mut issues, flow);
    check_size(&mut issues, flow);
    check_node_ids(&mut issues, flow);
    check_edges(&mut issues, flow);
    check_acyclic(&mut issues, flow);
    check_fan_in(&mut issues, flow);
    check_entries(&mut issues, flow);
    check_isolated(&mut issues, flow);

    let resolved = resolve_nodes(&mut issues, flow, available);
    check_contracts(&mut issues, flow, &resolved);
    check_timeouts(&mut issues, flow);

    issues.sort();
    issues
}

/// 解析每个节点的目标插件版本。解析不出来的节点会被记进 issues。
fn resolve_nodes(
    issues: &mut Vec<FlowIssue>,
    flow: &FlowDefinition,
    available: &BTreeMap<String, PluginAvailability>,
) -> HashMap<String, ResolvedNode> {
    let mut resolved = HashMap::new();

    for node in &flow.nodes {
        if node.plugin.trim().is_empty() {
            issues.push(FlowIssue::error(
                Code::PluginUnresolved,
                &node.id,
                format!("节点 {} 没有指定插件", node.id),
            ));
            continue;
        }

        let Some(avail) = available.get(&node.plugin) else {
            issues.push(FlowIssue::error(
                Code::PluginUnresolved,
                &node.id,
                format!("节点 {} 用的插件 {} 未注册", node.id, node.plugin),
            ));
            continue;
        };

        let Some(version) = pick_plugin_version(node.version.as_deref(), &avail.versions) else {
            issues.push(FlowIssue::error(
                Code::PluginUnresolved,
                &node.id,
                format!(
                    "节点 {} 要的 {}@{} 不存在——该插件当前有 [{}]",
                    node.id,
                    node.plugin,
                    node.version.as_deref().unwrap_or("最新"),
                    avail
                        .versions
                        .iter()
                        .map(|v| v.version.as_str())
                        .collect::<Vec<_>>()
                        .join("、"),
                ),
            ));
            continue;
        };

        resolved.insert(
            node.id.clone(),
            ResolvedNode {
                plugin: avail.plugin.clone(),
                version: version.version.clone(),
                produces: version.produces.clone(),
                consumes: version.consumes.clone(),
            },
        );
    }

    resolved
}

fn pick_plugin_version<'a>(
    constraint: Option<&str>,
    versions: &'a [PluginVersion],
) -> Option<&'a PluginVersion> {
    match constraint {
        None => versions.first(),
        Some(wanted) => versions.iter().find(|v| v.version == wanted),
    }
}

/// 从某个插件的可用版本里挑出满足约束的那个。
///
/// `available` 需按**新到旧**排序。M2 只支持两种约束：不指定（取最新的）与精确版本。
/// 语义化范围（`^1.2`）留到 flow 版本管理那一步——先支持简单形式，
/// 好过做一个半吊子的范围解析让人以为它能用。
pub fn pick_version<'a>(constraint: Option<&str>, available: &'a [String]) -> Option<&'a str> {
    match constraint {
        None => available.first().map(String::as_str),
        Some(wanted) => available
            .iter()
            .find(|v| v.as_str() == wanted)
            .map(String::as_str),
    }
}

// ---------------------------------------------------------------- 各项检查

fn check_name(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    if flow.name.trim().is_empty() {
        issues.push(FlowIssue::error(Code::InvalidName, "", "flow 缺少名字"));
        return;
    }
    if !is_valid_name(&flow.name) {
        issues.push(FlowIssue::error(
            Code::InvalidName,
            &flow.name,
            format!(
                "flow 名 {} 非法：只允许字母数字与 -_，最长 64 字符，且以字母数字开头",
                flow.name
            ),
        ));
    }
}

fn check_size(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    if flow.nodes.is_empty() {
        issues.push(FlowIssue::error(
            Code::NoNodes,
            &flow.name,
            "flow 至少要有一个节点",
        ));
        return;
    }
    if flow.nodes.len() > MAX_NODES {
        issues.push(FlowIssue::error(
            Code::TooManyNodes,
            &flow.name,
            format!(
                "节点数 {} 超过上限 {MAX_NODES}——这么大的编排没人看得懂，拆成多条 flow 或改用异步链",
                flow.nodes.len()
            ),
        ));
    }
}

fn check_node_ids(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    let mut seen: HashSet<&str> = HashSet::new();

    for node in &flow.nodes {
        if node.id.trim().is_empty() {
            issues.push(FlowIssue::error(
                Code::InvalidName,
                &flow.name,
                "存在没有 id 的节点",
            ));
            continue;
        }
        if !is_valid_name(&node.id) {
            issues.push(FlowIssue::error(
                Code::InvalidName,
                &node.id,
                format!(
                    "节点 id {} 非法：只允许字母数字与 -_，最长 64 字符",
                    node.id
                ),
            ));
        }
        if !seen.insert(node.id.as_str()) {
            issues.push(FlowIssue::error(
                Code::DuplicateNodeId,
                &node.id,
                format!(
                    "节点 id {} 重复——它会出现在调用链与日志里，必须唯一",
                    node.id
                ),
            ));
        }
    }
}

fn check_edges(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    let ids: HashSet<&str> = flow.nodes.iter().map(|n| n.id.as_str()).collect();

    for edge in &flow.edges {
        if !ids.contains(edge.from.as_str()) {
            issues.push(FlowIssue::error(
                Code::UnknownNode,
                &edge.from,
                format!("边引用了不存在的上游节点 {}", edge.from),
            ));
        }
        if !ids.contains(edge.to.as_str()) {
            issues.push(FlowIssue::error(
                Code::UnknownNode,
                &edge.to,
                format!("边引用了不存在的下游节点 {}", edge.to),
            ));
        }
        if edge.from == edge.to {
            issues.push(FlowIssue::error(
                Code::Cycle,
                &edge.from,
                format!("节点 {} 连到了自己", edge.from),
            ));
        }
    }
}

/// 环检测：拓扑排序走不完就说明有环。
fn check_acyclic(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    let ids: Vec<&str> = flow.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut in_degree: HashMap<&str, usize> = ids.iter().map(|id| (*id, 0)).collect();
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();

    for edge in &flow.edges {
        if !in_degree.contains_key(edge.from.as_str()) || !in_degree.contains_key(edge.to.as_str())
        {
            continue; // 端点不存在的边已由 check_edges 报过
        }
        *in_degree.get_mut(edge.to.as_str()).expect("已确认存在") += 1;
        outgoing
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }

    let mut ready: Vec<&str> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0;

    while let Some(current) = ready.pop() {
        visited += 1;
        for next in outgoing.get(current).into_iter().flatten() {
            let degree = in_degree.get_mut(next).expect("已确认存在");
            *degree -= 1;
            if *degree == 0 {
                ready.push(next);
            }
        }
    }

    if visited < in_degree.len() {
        let stuck: Vec<&str> = in_degree
            .iter()
            .filter(|(_, d)| **d > 0)
            .map(|(id, _)| *id)
            .collect();
        issues.push(FlowIssue::error(
            Code::Cycle,
            &flow.name,
            format!("编排里有环，涉及节点：{}", stuck.join("、")),
        ));
    }
}

/// 汇聚节点（入度 >1）在 M2 不支持。
///
/// 汇聚的语义不是「等它们都完成」那么简单——还要回答「合并后的信封以谁的载荷为准」。
/// 与其塞一个含糊的规则让人踩坑，不如先不支持，等真正需要时把合并规则设计清楚。
fn check_fan_in(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &flow.edges {
        incoming
            .entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }

    let mut fan_in: Vec<(&str, Vec<&str>)> = incoming
        .into_iter()
        .filter(|(_, froms)| froms.len() > 1)
        .collect();
    fan_in.sort();

    for (node, froms) in fan_in {
        issues.push(FlowIssue::error(
            Code::FanIn,
            node,
            format!(
                "节点 {node} 有多个上游（{}）——M2 的同步链不支持汇聚。\
                 汇聚要回答「合并后的信封以谁的载荷为准」，这个规则还没定；\
                 需要它时请拆成两条 flow，或等异步链",
                froms.join("、")
            ),
        ));
    }
}

fn check_entries(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    if !flow.nodes.is_empty() && flow.entry_nodes().is_empty() {
        issues.push(FlowIssue::error(
            Code::NoEntry,
            &flow.name,
            "没有入口节点（每个节点都有上游）——数据进不来",
        ));
    }
}

fn check_isolated(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    if flow.nodes.len() < 2 {
        return;
    }

    let mut connected: HashSet<&str> = HashSet::new();
    for edge in &flow.edges {
        connected.insert(edge.from.as_str());
        connected.insert(edge.to.as_str());
    }

    for node in &flow.nodes {
        if !connected.contains(node.id.as_str()) {
            issues.push(FlowIssue::warn(
                Code::IsolatedNode,
                &node.id,
                format!("节点 {} 没有跟任何节点相连，孤立的节点不会被执行", node.id),
            ));
        }
    }
}

/// 逐边检查契约能否接上。
///
/// 判定很简单：上游产出的消息类型，必须在下游消费的类型里出现。
/// 对不上就是接错线——这条检查把「上线后才发现某个节点收不到数据」提前到保存时。
fn check_contracts(
    issues: &mut Vec<FlowIssue>,
    flow: &FlowDefinition,
    resolved: &HashMap<String, ResolvedNode>,
) {
    for edge in &flow.edges {
        let (Some(from), Some(to)) = (resolved.get(&edge.from), resolved.get(&edge.to)) else {
            continue; // 节点没解析出来，已由 resolve_nodes 报过
        };

        let produced: HashSet<&str> = from.produces.iter().map(String::as_str).collect();
        let matched = to.consumes.iter().any(|fq| produced.contains(fq.as_str()));

        if !matched {
            issues.push(FlowIssue::error(
                Code::ContractMismatch,
                format!("{} → {}", edge.from, edge.to),
                format!(
                    "接不上：{}@{} 生产 [{}]，而 {}@{} 消费 [{}]，没有交集",
                    from.plugin,
                    from.version,
                    join_or_none(&from.produces),
                    to.plugin,
                    to.version,
                    join_or_none(&to.consumes),
                ),
            ));
        }
    }
}

fn check_timeouts(issues: &mut Vec<FlowIssue>, flow: &FlowDefinition) {
    for node in &flow.nodes {
        let Some(timeout) = node.timeout_ms else {
            continue;
        };
        if timeout <= 0 {
            issues.push(FlowIssue::error(
                Code::TimeoutTooLarge,
                &node.id,
                format!("节点 {} 的 timeout_ms 必须为正数", node.id),
            ));
        } else if timeout > MAX_NODE_TIMEOUT_MS {
            issues.push(FlowIssue::warn(
                Code::TimeoutTooLarge,
                &node.id,
                format!(
                    "节点 {} 的超时 {}ms 超过建议上限 {MAX_NODE_TIMEOUT_MS}ms——\
                     这么长的同步等待多半该改成异步处理",
                    node.id, timeout
                ),
            ));
        }
    }
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "（未声明）".to_string()
    } else {
        items.join("、")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{Edge, Node};

    fn node(id: &str, plugin: &str) -> Node {
        Node {
            id: id.to_string(),
            plugin: plugin.to_string(),
            version: None,
            timeout_ms: None,
            retries: None,
        }
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    const STRUCT: &str = "google.protobuf.Struct";

    fn version(v: &str, produces: &[&str], consumes: &[&str]) -> PluginVersion {
        PluginVersion {
            version: v.to_string(),
            produces: produces.iter().map(|s| (*s).to_string()).collect(),
            consumes: consumes.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn availability(plugin: &str, versions: Vec<PluginVersion>) -> PluginAvailability {
        PluginAvailability {
            plugin: plugin.to_string(),
            versions,
        }
    }

    /// 一条 auth → validate 的最小可跑 flow：两边都是 JSON 载荷。
    fn good_flow() -> FlowDefinition {
        FlowDefinition {
            name: "order-intake".to_string(),
            description: String::new(),
            nodes: vec![node("auth", "auth"), node("validate", "order-validator")],
            edges: vec![edge("auth", "validate")],
        }
    }

    /// 契约表按**插件名**索引——版本约束由校验器解析，所以它要看到每个插件有哪些版本。
    fn good_contracts() -> BTreeMap<String, PluginAvailability> {
        BTreeMap::from([
            (
                "auth".to_string(),
                availability("auth", vec![version("1.0.0", &[STRUCT], &[STRUCT])]),
            ),
            (
                "order-validator".to_string(),
                availability(
                    "order-validator",
                    vec![version("1.0.0", &[STRUCT], &[STRUCT])],
                ),
            ),
        ])
    }

    fn codes(issues: &[FlowIssue]) -> Vec<Code> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn 合规的编排没有错误() {
        let issues = validate(&good_flow(), &good_contracts());
        assert!(!has_errors(&issues), "不该有错误：{issues:?}");
        assert!(issues.is_empty(), "也不该有警告：{issues:?}");
    }

    #[test]
    fn 缺少节点被拦下() {
        let mut flow = good_flow();
        flow.nodes.clear();
        flow.edges.clear();

        let issues = validate(&flow, &BTreeMap::new());
        assert_eq!(codes(&issues), vec![Code::NoNodes]);
    }

    #[test]
    fn 节点数超上限被拦下() {
        let mut flow = good_flow();
        flow.edges.clear();
        flow.nodes = (0..MAX_NODES + 1)
            .map(|i| node(&format!("n{i}"), "auth"))
            .collect();

        let issues = validate(&flow, &good_contracts());
        assert!(codes(&issues).contains(&Code::TooManyNodes));
    }

    #[test]
    fn 节点_id_重复被拦下() {
        let mut flow = good_flow();
        flow.edges.clear();
        flow.nodes = vec![node("dup", "auth"), node("dup", "auth")];

        let issues = validate(&flow, &good_contracts());
        assert!(codes(&issues).contains(&Code::DuplicateNodeId));
    }

    #[test]
    fn 边引用不存在的节点被拦下() {
        let mut flow = good_flow();
        flow.edges.push(edge("validate", "没这个节点"));

        let issues = validate(&flow, &good_contracts());
        let unknown: Vec<&FlowIssue> = issues
            .iter()
            .filter(|i| i.code == Code::UnknownNode)
            .collect();
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].detail.contains("没这个节点"));
    }

    #[test]
    fn 有环被拦下() {
        let mut flow = good_flow();
        flow.edges.push(edge("validate", "auth"));

        let issues = validate(&flow, &good_contracts());
        assert!(
            codes(&issues).contains(&Code::Cycle),
            "成环必须被拦：{issues:?}"
        );
    }

    #[test]
    fn 自环被拦下() {
        let mut flow = good_flow();
        flow.edges.push(edge("auth", "auth"));

        let issues = validate(&flow, &good_contracts());
        assert!(codes(&issues).contains(&Code::Cycle));
    }

    #[test]
    fn 汇聚节点被拦下并说明原因() {
        let mut flow = good_flow();
        flow.nodes.push(node("audit", "auth"));
        // validate 现在有两个上游
        flow.edges.push(edge("audit", "validate"));

        let issues = validate(&flow, &good_contracts());
        let fan_in = issues
            .iter()
            .find(|i| i.code == Code::FanIn)
            .expect("应报汇聚");
        assert_eq!(fan_in.subject, "validate");
        assert!(
            fan_in.detail.contains("以谁的载荷为准"),
            "要说清楚为什么现在不支持，实际 {}",
            fan_in.detail
        );
    }

    #[test]
    fn 插件未注册被拦下且指出插件名() {
        let mut contracts = good_contracts();
        contracts.remove("order-validator");

        let issues = validate(&good_flow(), &contracts);
        let unresolved = issues
            .iter()
            .find(|i| i.code == Code::PluginUnresolved)
            .expect("应报插件未注册");
        assert_eq!(unresolved.subject, "validate");
        assert!(unresolved.detail.contains("order-validator"));
    }

    #[test]
    fn 版本约束解析不出来时列出可用版本() {
        let mut flow = good_flow();
        flow.nodes[1].version = Some("9.9.9".to_string());

        let issues = validate(&flow, &good_contracts());
        let unresolved = issues
            .iter()
            .find(|i| i.code == Code::PluginUnresolved)
            .expect("应报版本解析失败");
        assert!(
            unresolved.detail.contains("order-validator@9.9.9"),
            "应指出要的是哪个版本，实际 {}",
            unresolved.detail
        );
        assert!(
            unresolved.detail.contains("1.0.0"),
            "应列出可用的版本，实际 {}",
            unresolved.detail
        );
    }

    #[test]
    fn 版本约束能解析时按指定版本取其契约() {
        // 插件有两个版本，各自契约不同；节点锁定旧的那一个
        let mut contracts = good_contracts();
        contracts.insert(
            "order-validator".to_string(),
            availability(
                "order-validator",
                vec![
                    version("2.0.0", &["wms.v1.OrderCheckedV2"], &[STRUCT]),
                    version("1.0.0", &["wms.v1.OrderChecked"], &["wms.v1.OrderCreated"]),
                ],
            ),
        );
        contracts.insert(
            "auth".to_string(),
            availability(
                "auth",
                vec![version("1.0.0", &["wms.v1.OrderCreated"], &[STRUCT])],
            ),
        );

        let mut flow = good_flow();
        flow.nodes[1].version = Some("1.0.0".to_string());

        let issues = validate(&flow, &contracts);
        assert!(
            !has_errors(&issues),
            "锁 1.0.0 时应按 1.0.0 的契约判定，能接上：{issues:?}"
        );
    }

    #[test]
    fn 契约接不上被拦下并列出两边的类型() {
        let mut contracts = good_contracts();
        contracts.insert(
            "order-validator".to_string(),
            availability(
                "order-validator",
                vec![version(
                    "1.0.0",
                    &["wms.v1.OrderChecked"],
                    &["wms.v1.OrderCreated"],
                )],
            ),
        );

        let issues = validate(&good_flow(), &contracts);
        let mismatch = issues
            .iter()
            .find(|i| i.code == Code::ContractMismatch)
            .expect("应报契约不匹配");
        assert_eq!(mismatch.subject, "auth → validate");
        assert!(
            mismatch.detail.contains(STRUCT),
            "应列出上游产出，实际 {}",
            mismatch.detail
        );
        assert!(
            mismatch.detail.contains("wms.v1.OrderCreated"),
            "应列出下游消费，实际 {}",
            mismatch.detail
        );
    }

    #[test]
    fn 类型化契约能正确接上() {
        let contracts = BTreeMap::from([
            (
                "auth".to_string(),
                availability(
                    "auth",
                    vec![version("1.0.0", &["wms.v1.OrderCreated"], &[STRUCT])],
                ),
            ),
            (
                "order-validator".to_string(),
                availability(
                    "order-validator",
                    vec![version(
                        "1.0.0",
                        &["wms.v1.OrderChecked"],
                        &["wms.v1.OrderCreated"],
                    )],
                ),
            ),
        ]);

        let issues = validate(&good_flow(), &contracts);
        assert!(!has_errors(&issues), "应能接上：{issues:?}");
    }

    #[test]
    fn 未指定版本时取最新的那个() {
        // 最新版产出的类型与下游对不上，旧版才对得上——未锁版本时应当按最新版判定并报错
        let contracts = BTreeMap::from([
            (
                "auth".to_string(),
                availability(
                    "auth",
                    vec![version("1.0.0", &["wms.v1.OrderCreated"], &[STRUCT])],
                ),
            ),
            (
                "order-validator".to_string(),
                availability(
                    "order-validator",
                    vec![
                        version(
                            "2.0.0",
                            &["wms.v1.OrderCheckedV2"],
                            &["wms.v1.OrderCreatedV2"],
                        ),
                        version("1.0.0", &["wms.v1.OrderChecked"], &["wms.v1.OrderCreated"]),
                    ],
                ),
            ),
        ]);

        let issues = validate(&good_flow(), &contracts);
        assert!(
            codes(&issues).contains(&Code::ContractMismatch),
            "未锁版本时应按最新的 2.0.0 判定，接不上：{issues:?}"
        );
    }

    #[test]
    fn 孤立节点只告警不拦截() {
        let mut flow = good_flow();
        flow.nodes.push(node("orphan", "auth"));

        let issues = validate(&flow, &good_contracts());
        assert!(!has_errors(&issues), "孤立节点不该拦下保存");
        let orphan = issues
            .iter()
            .find(|i| i.code == Code::IsolatedNode)
            .expect("应给出提示");
        assert_eq!(orphan.severity, Severity::Warning);
    }

    #[test]
    fn 单个节点的_flow_不算孤立() {
        let flow = FlowDefinition {
            name: "single".to_string(),
            description: String::new(),
            nodes: vec![node("only", "auth")],
            edges: vec![],
        };

        let issues = validate(&flow, &good_contracts());
        assert!(!codes(&issues).contains(&Code::IsolatedNode));
    }

    #[test]
    fn 过长的节点超时只告警() {
        let mut flow = good_flow();
        flow.nodes[0].timeout_ms = Some(MAX_NODE_TIMEOUT_MS + 1);

        let issues = validate(&flow, &good_contracts());
        assert!(!has_errors(&issues), "只是建议，不该拦下");
        assert!(codes(&issues).contains(&Code::TimeoutTooLarge));
    }

    #[test]
    fn 非正的节点超时被拦下() {
        let mut flow = good_flow();
        flow.nodes[0].timeout_ms = Some(0);

        let issues = validate(&flow, &good_contracts());
        assert!(codes(&issues).contains(&Code::TimeoutTooLarge));
        assert!(has_errors(&issues));
    }

    #[test]
    fn flow_名非法被拦下() {
        let mut flow = good_flow();
        flow.name = "有中文".to_string();

        let issues = validate(&flow, &good_contracts());
        assert!(codes(&issues).contains(&Code::InvalidName));
    }

    #[test]
    fn 结果顺序稳定() {
        let mut flow = good_flow();
        flow.name = "非法 名".to_string();
        flow.edges.push(edge("validate", "没这个节点"));
        flow.nodes[0].timeout_ms = Some(-1);

        let first = validate(&flow, &good_contracts());
        let second = validate(&flow, &good_contracts());
        assert_eq!(first, second, "排序必须稳定，否则错误提示会随机漂移");
    }

    #[test]
    fn 汇总只统计错误() {
        let mut flow = good_flow();
        flow.nodes.push(node("orphan", "auth"));

        let issues = validate(&flow, &good_contracts());
        assert!(summarize(&issues).is_none(), "只有警告时不该有错误汇总");

        flow.nodes[0].timeout_ms = Some(-1);
        let with_error = validate(&flow, &good_contracts());
        assert!(summarize(&with_error).is_some());
    }

    #[test]
    fn 挑版本_未指定时取最新() {
        let available = vec![
            "2.0.0".to_string(),
            "1.1.0".to_string(),
            "1.0.0".to_string(),
        ];
        assert_eq!(pick_version(None, &available), Some("2.0.0"));
        assert_eq!(pick_version(Some("1.1.0"), &available), Some("1.1.0"));
        assert_eq!(pick_version(Some("9.9.9"), &available), None);
        assert_eq!(pick_version(None, &[]), None);
    }
}
