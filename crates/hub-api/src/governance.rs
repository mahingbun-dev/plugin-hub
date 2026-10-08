//! 治理面：实例级的并发、熔断与背压状态。
//!
//! 这些数字全在**中台进程的内存里**（见 `hub_engine::govern`），不落库：
//! 它们是「此刻」的事实，存下来只会是一份很快过期的快照。
//! 代价是**多实例部署时每个中台只知道自己那一份**——所以响应里带着配置，
//! 让看的人知道这些上限是怎么来的。

use axum::Json;
use axum::extract::State;
use hub_engine::govern::InstanceGovernance;
use serde::Serialize;

use crate::ApiState;
use crate::error::ApiError;

pub async fn governance(
    State(state): State<ApiState>,
) -> Result<Json<GovernanceResponse>, ApiError> {
    let governor = state.invoker.governor();
    let config = governor.config();

    Ok(Json(GovernanceResponse {
        config: GovernConfigDto {
            max_concurrency: config.max_concurrency,
            queue_timeout_ms: config.queue_timeout.as_millis() as u64,
            failure_threshold: config.failure_threshold,
            open_cooldown_secs: config.open_cooldown.as_secs(),
        },
        instances: governor.snapshot().into_iter().map(Into::into).collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct GovernanceResponse {
    pub config: GovernConfigDto,

    /// 此刻有治理记录的实例。
    ///
    /// ⚠️ **这不等于「在线实例」**：治理表是按「被调用过」建项的，
    /// 一个刚注册、还没被调用过的实例不会出现在这里；反过来，
    /// 已下线但还留着失败计数的实例会（那正是要保留它的原因）。
    /// 要判断「谁在线」看 `/admin/instances`，两者口径不同，不能互相替代。
    pub instances: Vec<InstanceGovernanceDto>,
}

#[derive(Debug, Serialize)]
pub struct GovernConfigDto {
    pub max_concurrency: usize,
    pub queue_timeout_ms: u64,
    pub failure_threshold: u32,
    pub open_cooldown_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct InstanceGovernanceDto {
    pub instance_id: String,
    pub max_concurrency: usize,
    pub available: usize,
    pub in_flight: usize,

    /// `closed` / `open` / `probing`
    pub breaker: String,

    /// 距离冷却结束还有多少毫秒；未跳闸时不给这个字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_for_ms: Option<u64>,

    /// 连续失败次数。跳闸后不清零，只有半开探测成功才归零
    pub consecutive_failures: u32,

    /// 累计放行过的调用数
    pub admitted: u64,
}

impl From<InstanceGovernance> for InstanceGovernanceDto {
    fn from(item: InstanceGovernance) -> Self {
        Self {
            instance_id: item.instance_id,
            max_concurrency: item.max_concurrency,
            available: item.available,
            in_flight: item.in_flight,
            breaker: match item.phase {
                hub_engine::govern::BreakerPhase::Closed => "closed",
                hub_engine::govern::BreakerPhase::Open => "open",
                hub_engine::govern::BreakerPhase::Probing => "probing",
            }
            .to_string(),
            open_for_ms: item.open_for.map(|d| d.as_millis() as u64),
            consecutive_failures: item.consecutive_failures,
            admitted: item.admitted,
        }
    }
}
