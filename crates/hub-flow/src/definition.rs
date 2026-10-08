//! flow 定义（编排 DSL）。
//!
//! 一条 flow 是一张 DAG：节点是插件调用，边是数据流向。
//! 这份结构是**中台与人的共同语言**——控制台的编排画布、MCP 工具、校验器都围绕它。

use serde::{Deserialize, Serialize};

/// 单条 flow 的节点数上限。
///
/// 32 是刻意的：编排是给人看、给 agent 读的，超过这个规模就该拆成多条 flow
/// 或用异步链，而不是画一张谁也看不懂的图。
pub const MAX_NODES: usize = 32;

/// flow 名与节点 id 的字符集：要能进 URL、能当 MCP 工具名的一部分。
pub const NAME_PATTERN: &str = "^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$";

/// 名字是否符合 [`NAME_PATTERN`] 的语义。
///
/// 正则解析一次的开销不值当，手写判断；flow 名与节点 id 的校验共用
/// 这一个函数，两边靠同一个测试守住。
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 一条编排。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowDefinition {
    /// flow 名，全局唯一
    pub name: String,

    #[serde(default)]
    pub description: String,

    pub nodes: Vec<Node>,

    /// 边。**没有入边的节点是入口**，会收到触发时那份信封的副本。
    #[serde(default)]
    pub edges: Vec<Edge>,
}

/// 一次插件调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// 节点 id，flow 内唯一。它会出现在调用链、日志与 span 里，取个能读懂的名字。
    pub id: String,

    /// 插件名
    pub plugin: String,

    /// 版本约束；缺省表示用该插件的最新版本。
    ///
    /// 锁定版本是灰度的基础：升级插件时先起新版本副本，再把这里的约束抬高。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// 节点超时（毫秒）。
    ///
    /// 它是**上限**而不是保证：真正生效的是它与信封剩余预算中更小的那个。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,

    /// 失败重试次数（不含首次），缺省 0。
    ///
    /// 只对「可重试的失败」生效——超时不重试（下游多半已经忙不过来了），
    /// 校验拒绝也不重试（同样的数据重发一次结果还是拒绝）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

/// 一条边：`from` 的输出喂给 `to`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

impl FlowDefinition {
    /// 按声明的顺序返回入口节点（没有入边的那些）。
    pub fn entry_nodes(&self) -> Vec<&str> {
        let mut has_incoming = std::collections::HashSet::new();
        for edge in &self.edges {
            has_incoming.insert(edge.to.as_str());
        }
        self.nodes
            .iter()
            .map(|n| n.id.as_str())
            .filter(|id| !has_incoming.contains(id))
            .collect()
    }

    /// 某个节点的上游（按边声明顺序）。
    pub fn upstreams(&self, node_id: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|e| e.to == node_id)
            .map(|e| e.from.as_str())
            .collect()
    }

    pub fn node(&self, node_id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> FlowDefinition {
        FlowDefinition {
            name: "order-intake".to_string(),
            description: String::new(),
            nodes: vec![
                Node {
                    id: "auth".to_string(),
                    plugin: "auth".to_string(),
                    version: None,
                    timeout_ms: None,
                    retries: None,
                },
                Node {
                    id: "validate".to_string(),
                    plugin: "order-validator".to_string(),
                    version: None,
                    timeout_ms: None,
                    retries: None,
                },
                Node {
                    id: "audit".to_string(),
                    plugin: "audit".to_string(),
                    version: None,
                    timeout_ms: None,
                    retries: None,
                },
            ],
            edges: vec![
                Edge {
                    from: "auth".to_string(),
                    to: "validate".to_string(),
                },
                // 扇出：validate 与 audit 都从 auth 拿数据
                Edge {
                    from: "auth".to_string(),
                    to: "audit".to_string(),
                },
            ],
        }
    }

    #[test]
    fn 没有入边的节点就是入口() {
        assert_eq!(flow().entry_nodes(), vec!["auth"]);
    }

    #[test]
    fn 无边的_flow_所有节点都是入口() {
        let mut f = flow();
        f.edges.clear();
        assert_eq!(f.entry_nodes(), vec!["auth", "validate", "audit"]);
    }

    #[test]
    fn 上游按边声明顺序返回() {
        assert_eq!(flow().upstreams("validate"), vec!["auth"]);
        assert_eq!(flow().upstreams("auth"), Vec::<&str>::new());
    }

    #[test]
    fn 可按_id_查节点() {
        assert_eq!(
            flow().node("validate").map(|n| n.plugin.as_str()),
            Some("order-validator")
        );
        assert!(flow().node("不存在").is_none());
    }

    #[test]
    fn 定义可_json_往返() {
        let f = flow();
        let json = serde_json::to_string(&f).expect("序列化失败");
        let back: FlowDefinition = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(f, back);
    }
}
