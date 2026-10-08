//! 注册拒绝原因的构造。
//!
//! 抽出来是为了让**别的东西也能跑同一套判定**：`hub-mock` 是给开发者的本地替身，
//! 它必须与真中台判出一样的结果。如果这些措辞留在 `Registry::try_register` 里，
//! 替身就只能照抄一遍，而照抄的副本迟早与原件漂移——本仓库已经吃过一次这个亏
//! （`sdk/go/conformance/conformance.go` 里有一份中台措辞的手抄副本）。
//!
//! 这里只放**不需要访问存储**的那几条（可达性探测、实例标识）。契约兼容那几条要
//! 读历史版本基线，留在 `try_register` 里——但它们用到的比对函数
//! （[`hub_contract::check_compatibility`] 等）本来就是公开的，替身直接调用即可。

use hub_contract::ContractIndex;
use hub_proto::v1::{PluginManifest, RegisterRequest, RejectCode, Rejection};

use crate::probe::ProbeOutcome;
use crate::{MAX_INSTANCE_ID_LEN, validate};

/// 插件不可达/不健康时该回什么。
///
/// `None` 表示探测通过、可以继续。
///
/// 探测不通过的两种成因要给不同的下一步，不能合并成一句「请检查配置」：
/// 连不上是地址/网络写错了，而「连上了但自报不健康」是插件自己的健康检查没过，
/// 两者的排查方向完全不同，指错方向会让插件方在网络上白费半天。
/// `message` 也跟着分开：把后一种说成「地址不可达」，等于把人往网络那个方向带。
pub fn probe_rejection(advertise_addr: &str, outcome: &ProbeOutcome) -> Option<Rejection> {
    if outcome.is_healthy() {
        return None;
    }

    let (message, guidance) = match outcome {
        ProbeOutcome::Unhealthy { .. } => (
            format!("插件地址 {advertise_addr} 上的实例自报不健康"),
            "中台拨通了该地址，但插件的 Health 返回不健康，注册因此不通过；\
             请查插件自身的健康检查逻辑与它依赖的下游",
        ),
        // is_healthy() 已经拦掉剩下的一种，这里只可能是连不上
        _ => (
            format!("插件地址 {advertise_addr} 不可达"),
            "中台要能主动拨通这个地址才准注册：请确认 advertise_addr 填的是\
             中台视角下可达的地址（SDK 里是 HUB_ADVERTISE_ADDR）——中台在别的\
             机器时写 localhost 会指到中台自己，要写内网 IP 或域名",
        ),
    };

    Some(Rejection {
        code: RejectCode::Unreachable as i32,
        message,
        detail: format!("{}；{guidance}", outcome.message()),
    })
}

/// 校验 `instance_id` 的**格式**：空、超长。不需要查库。
///
/// 拆成两个函数是为了让「查库那一步」只在格式没问题时才发生——
/// 属主查询是这条链上唯一一次额外的数据库往返。
pub fn instance_id_format_rejection(instance_id: &str) -> Option<Rejection> {
    if instance_id.trim().is_empty() {
        return Some(Rejection {
            code: RejectCode::InstanceConflict as i32,
            message: "注册请求缺少 instance_id".to_string(),
            detail: "instance_id 不能为空：实例行与状态凭证都挂在它上面；用 hubkit 时\
                     别把 HUB_INSTANCE_ID 设成空白，不设它反而会由 SDK 自动生成\
                     「主机名-PID」"
                .to_string(),
        });
    }

    if instance_id.len() > MAX_INSTANCE_ID_LEN {
        return Some(Rejection {
            code: RejectCode::InstanceConflict as i32,
            message: format!("instance_id 过长（{} 字节）", instance_id.len()),
            detail: format!("上限 {MAX_INSTANCE_ID_LEN} 字节，请用「主机名-PID」这类短标识"),
        });
    }

    None
}

/// 校验 `instance_id` 的**属主**。`owner` 是当前占用它的插件名（没占用就传 `None`）。
///
/// `instance_id` 完全由插件自己生成、中台此前不做任何校验，但它同时是
/// `plugin_instances` 的唯一键与状态凭证的载体。若放行「B 拿 A 的 instance_id 注册」，
/// upsert 会把那一行的 version_id 改成 B 的并轮换凭证：A 从自己插件的实例列表里
/// 消失（选中它时 NoHealthyInstance），而 A 的心跳仍按 instance_id 命中、
/// 完全察觉不到丢了注册——两边于是每次重注册都互相顶掉，永久空转。
/// 所以先查属主，属主是别的插件就拒绝；同一插件（换版本是升级、同版本是重启）
/// 放行，实例行随 upsert 正常迁移。
pub fn instance_id_owner_rejection(
    instance_id: &str,
    plugin: &str,
    owner: Option<&str>,
) -> Option<Rejection> {
    match owner {
        Some(owner) if owner != plugin => Some(Rejection {
            code: RejectCode::InstanceConflict as i32,
            message: format!("instance_id {instance_id} 已被插件 {owner} 占用"),
            detail: "同一 instance_id 不能跨插件复用；请让每个插件生成自己的实例标识\
                     ——撞车多半是几个插件的 HUB_INSTANCE_ID 写成了同一个值，缺省\
                     「主机名-PID」在同一 host 网络下就会撞"
                .to_string(),
        }),
        _ => None,
    }
}

/// 注册请求最前面那几步的校验结果。
pub enum Preflight {
    /// 拦下了。**一次给全部问题**——真中台就是这么回的，
    /// 让插件方一次改完而不是来回试。
    Reject(Vec<Rejection>),

    /// 过了，把解析好的 manifest 与 descriptor 索引带出去给后面的步骤用。
    Ok {
        manifest: PluginManifest,
        index: ContractIndex,
    },
}

/// 跑注册流程里**不需要存储**的那几步：manifest 在不在、descriptor 能不能解析、
/// manifest 自不自洽。
///
/// 真中台与 `hub-mock` 都从这里开始，判据因此不可能分叉。
pub fn preflight(req: &RegisterRequest) -> Preflight {
    let Some(manifest) = req.manifest.clone() else {
        return Preflight::Reject(vec![Rejection {
            code: RejectCode::ManifestInvalid as i32,
            message: "注册请求缺少 manifest".to_string(),
            detail: "manifest 必须随注册一起提交；它是 RegisterRequest 的必填字段，\
                     hubkit 会自动填 Manifest() 的返回值，请确认该方法返回了完整定义"
                .to_string(),
        }]);
    };

    // descriptor 必须能解析——后面所有契约判断都建立在它之上
    let index = match ContractIndex::from_descriptor_set(&req.descriptor_set) {
        Ok(index) => index,
        Err(err) => {
            return Preflight::Reject(vec![Rejection {
                code: RejectCode::DescriptorInvalid as i32,
                message: "提交的 FileDescriptorSet 无法解析".to_string(),
                detail: format!(
                    "{err}；descriptor_set 要的是 proto 编译产物 FileDescriptorSet 的编码字节\
                     （不是 .proto 源码，也不是单个 FileDescriptorProto）——用 hubkit 时\
                     检查 Descriptor() 是否返回了它，缺省实现是 DescriptorOf()"
                ),
            }]);
        }
    };

    let rejections = validate::validate_manifest(&manifest, &index);
    if !rejections.is_empty() {
        return Preflight::Reject(rejections);
    }

    Preflight::Ok { manifest, index }
}
