//! 执行计划：把 DAG 排成可并发的「层」。
//!
//! 同一层里的节点互不依赖，可以并发执行；层与层之间串行。
//! 这是同步链执行引擎的输入——引擎只管照着层跑，不必自己理解图。

use std::collections::{BTreeSet, HashMap};

use crate::definition::FlowDefinition;

/// 一份执行计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// 外层是执行顺序，内层是可并发的一组节点 id（按声明顺序，便于复现）
    pub levels: Vec<Vec<String>>,

    /// 每个节点的上游（按边声明顺序），引擎据此决定「输入信封从哪来」
    pub upstreams: HashMap<String, Vec<String>>,

    /// 每个节点的下游。引擎靠它判断「谁是叶子」——叶子节点的输出就是整条链的输出。
    pub successors: HashMap<String, Vec<String>>,
}

impl ExecutionPlan {
    /// 节点总数。
    pub fn node_count(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }

    /// 最宽的一层有几个节点——并发度的上界。
    pub fn max_width(&self) -> usize {
        self.levels.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// 某个节点的第一个上游（M2 已保证入度 ≤1，所以最多一个）。
    pub fn upstream_of(&self, node_id: &str) -> Option<&str> {
        self.upstreams
            .get(node_id)
            .and_then(|ups| ups.first())
            .map(String::as_str)
    }

    /// 某个节点是不是叶子（没有下游）。
    pub fn is_leaf(&self, node_id: &str) -> bool {
        self.successors
            .get(node_id)
            .is_none_or(|next| next.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("编排里有环，排不出执行顺序；涉及节点：{nodes}")]
    Cycle { nodes: String },

    #[error("编排里没有节点")]
    Empty,
}

/// 把编排排成执行计划。
///
/// 入参应当已经过 [`crate::validate`]；这里仍会自己检环——静默产出一份跑不通的计划
/// 比报错危险得多。
pub fn plan(flow: &FlowDefinition) -> Result<ExecutionPlan, PlanError> {
    if flow.nodes.is_empty() {
        return Err(PlanError::Empty);
    }

    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut upstreams: HashMap<String, Vec<String>> = HashMap::new();

    for node in &flow.nodes {
        in_degree.insert(node.id.as_str(), 0);
    }
    for edge in &flow.edges {
        if !in_degree.contains_key(edge.from.as_str()) || !in_degree.contains_key(edge.to.as_str())
        {
            continue; // 端点不存在的边由校验阶段报错，这里跳过
        }
        *in_degree.get_mut(edge.to.as_str()).expect("已确认存在") += 1;
        outgoing
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
        upstreams
            .entry(edge.to.clone())
            .or_default()
            .push(edge.from.clone());
    }

    // 按声明顺序取同层节点，保证计划可复现
    let order: Vec<&str> = flow.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut remaining: BTreeSet<&str> = order.iter().copied().collect();
    let mut levels = Vec::new();
    let mut settled = 0;

    loop {
        let ready: Vec<String> = order
            .iter()
            .filter(|id| remaining.contains(**id))
            .filter(|id| in_degree.get(**id).copied().unwrap_or(0) == 0)
            .map(|id| (*id).to_string())
            .collect();

        if ready.is_empty() {
            break;
        }

        for id in &ready {
            remaining.remove(id.as_str());
            settled += 1;
        }
        for id in &ready {
            for next in outgoing.get(id.as_str()).into_iter().flatten() {
                let degree = in_degree.get_mut(next).expect("已确认存在");
                *degree -= 1;
            }
        }

        levels.push(ready);
    }

    if settled < flow.nodes.len() {
        let stuck: Vec<&str> = remaining.into_iter().collect();
        return Err(PlanError::Cycle {
            nodes: stuck.join("、"),
        });
    }

    Ok(ExecutionPlan {
        levels,
        upstreams,
        successors: outgoing
            .into_iter()
            .map(|(from, tos)| {
                (
                    from.to_string(),
                    tos.into_iter().map(str::to_string).collect(),
                )
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{Edge, Node};

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            plugin: "p".to_string(),
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

    fn flow(nodes: &[&str], edges: &[(&str, &str)]) -> FlowDefinition {
        FlowDefinition {
            name: "f".to_string(),
            description: String::new(),
            nodes: nodes.iter().map(|id| node(id)).collect(),
            edges: edges.iter().map(|(f, t)| edge(f, t)).collect(),
        }
    }

    #[test]
    fn 链式编排每层一个节点() {
        let p = plan(&flow(&["a", "b", "c"], &[("a", "b"), ("b", "c")])).expect("应成功");
        assert_eq!(p.levels, vec![vec!["a"], vec!["b"], vec!["c"]]);
        assert_eq!(p.max_width(), 1);
        assert_eq!(p.upstream_of("b"), Some("a"));
        assert_eq!(p.upstream_of("a"), None);
    }

    #[test]
    fn 扇出被排进同一层() {
        // a 之后并发跑 b 与 c
        let p = plan(&flow(&["a", "b", "c"], &[("a", "b"), ("a", "c")])).expect("应成功");
        assert_eq!(p.levels.len(), 2);
        assert_eq!(p.levels[0], vec!["a"]);
        assert_eq!(p.levels[1], vec!["b", "c"]);
        assert_eq!(p.max_width(), 2, "并发度上界应为 2");
    }

    #[test]
    fn 多个入口都是第一层() {
        let p = plan(&flow(&["a", "b", "c"], &[("a", "c"), ("b", "c")])).unwrap_or_else(|_| {
            // 汇聚在 M2 不被支持，但计划器本身要能排出顺序
            panic!("计划器不该因为汇聚而失败")
        });
        assert_eq!(p.levels[0], vec!["a", "b"]);
        assert_eq!(p.levels[1], vec!["c"]);
    }

    #[test]
    fn 同层顺序与声明一致() {
        let p = plan(&flow(&["c", "a", "b"], &[])).expect("应成功");
        assert_eq!(
            p.levels[0],
            vec!["c", "a", "b"],
            "按声明顺序，保证计划可复现"
        );
    }

    #[test]
    fn 有环时报错而不是静默产出跑不通的计划() {
        let err = plan(&flow(&["a", "b"], &[("a", "b"), ("b", "a")])).expect_err("应报错");
        match err {
            PlanError::Cycle { nodes } => {
                assert!(nodes.contains('a') && nodes.contains('b'), "实际 {nodes}");
            }
            other => panic!("应是环错误，实际 {other:?}"),
        }
    }

    #[test]
    fn 空编排报错() {
        assert_eq!(plan(&flow(&[], &[])).unwrap_err(), PlanError::Empty);
    }

    #[test]
    fn 孤立节点自成一层() {
        let p = plan(&flow(&["a", "b", "orphan"], &[("a", "b")])).expect("应成功");
        // orphan 没有依赖，与 a 同层
        assert_eq!(p.levels[0], vec!["a", "orphan"]);
        assert_eq!(p.levels[1], vec!["b"]);
    }

    #[test]
    fn 计划可复现() {
        let f = flow(&["a", "b", "c", "d"], &[("a", "b"), ("a", "c"), ("b", "d")]);
        assert_eq!(plan(&f), plan(&f));
    }

    #[test]
    fn 节点计数正确() {
        let p = plan(&flow(&["a", "b", "c"], &[("a", "b")])).expect("应成功");
        assert_eq!(p.node_count(), 3);
    }
}
