//! 注册发现：插件的自注册、心跳、摘除与实例选择。
//!
//! 注册流程是「插座」最要紧的一段，任何一步不通过都拒绝，且拒绝原因要能直接定位问题：
//!
//! 1. **manifest 自洽**（[`validate`]）—— 名字/版本/工具名/声明的消息类型
//! 2. **descriptor 可解析**（[`hub_contract::ContractIndex`]）
//! 3. **可达性探测**（[`PluginProbe`]）—— 避免「注册成功但永远调不通」
//! 4. **契约兼容**（[`hub_contract::check_compatibility`]）—— 相对基线有无破坏性变更
//! 5. **实例标识校验**（[`hub_store::instances::instance_owner`]）—— `instance_id` 不被别的插件占用
//! 6. 落库：插件 / 版本 / 契约 / 工具 / 实例
//!
//! 回给插件的每一条 [`Rejection`] 都要回答两个问题，缺一不可：`message` 说**哪里错了**
//! （点出出问题的那个东西），`detail` 说**该怎么办**（给插件方的下一步动作）。
//! 读者是插件开发者而不是中台的人，且他看到的是日志里的一行字：写「请检查配置」等于没说，
//! 得指名道姓到 `HUB_ADVERTISE_ADDR`、`Manifest()`、字段编号这一层。

pub mod probe;
pub mod reject;
pub mod validate;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use hub_contract::{ContractIndex, Severity, check_compatibility, has_breaking, summarize};
use hub_proto::v1::{HeartbeatResponse, RegisterRequest, RegisterResponse, RejectCode, Rejection};
use hub_store::Store;
use hub_store::plugins::{self, NewTool, NewVersion};
use hub_store::rejections;

pub use probe::{PluginProbe, ProbeOutcome};
pub use validate::is_valid_plugin_name;

/// `instance_id` 的长度上限。
///
/// 它是插件自报的字符串，会作为 `plugin_instances` 的唯一键落库。SDK 的缺省值是
/// `主机名-PID`（主机名最长 253），256 足够容纳真实取值，又能挡住把大块内容灌进这一列。
///
/// `pub` 是给 `hub-mock` 用的：那个本地替身要跑**同一套**校验，自己再抄一个常量
/// 就是等着它跟这里漂移。
pub const MAX_INSTANCE_ID_LEN: usize = 256;

/// 注册表运行参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryConfig {
    /// 中台指定、插件遵守的心跳周期
    pub heartbeat_interval_seconds: i32,

    /// 超过这个时长没收到心跳就摘除实例。
    ///
    /// 默认是心跳周期的 3 倍：偶发丢一两次心跳不该让实例下线。
    pub stale_after: Duration,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_seconds: 10,
            stale_after: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("存储错误: {0}")]
    Store(#[from] hub_store::StoreError),

    /// 已落库的 descriptor 解析失败，属数据损坏，不是插件的问题
    #[error("契约解析失败: {0}")]
    Contract(#[from] hub_contract::ContractError),

    #[error("插件 {0} 未注册")]
    PluginNotFound(String),

    #[error("插件 {plugin} 没有版本 {version}")]
    VersionNotFound { plugin: String, version: String },

    #[error("插件 {plugin} 当前没有可用实例（全部掉线或尚未注册）")]
    NoHealthyInstance { plugin: String },

    /// 实例刚 upsert 完却读不到凭证，说明写入与读取之间出了岔子。
    ///
    /// 单列成一个变体而不是复用 `StoreError`：这不是「数据库操作失败」，
    /// 而是「注册流程自身的后置条件没成立」——两者要排查的方向完全不同。
    #[error("实例 {0} 注册后取不到状态凭证")]
    MissingStateToken(String),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// 一次调用的目标实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceTarget {
    pub plugin_name: String,
    pub version: String,
    pub instance_id: String,
    pub advertise_addr: String,
}

/// 注册表。
#[derive(Clone)]
pub struct Registry {
    store: Store,
    probe: Arc<dyn PluginProbe>,
    cfg: RegistryConfig,
}

impl Registry {
    pub fn new(store: Store, probe: Arc<dyn PluginProbe>, cfg: RegistryConfig) -> Self {
        Self { store, probe, cfg }
    }

    pub fn config(&self) -> &RegistryConfig {
        &self.cfg
    }

    /// 供状态面复用同一个连接池。
    ///
    /// 注册面与状态面本就共享同一个库——状态面的凭证校验要查注册时落下的行。
    /// 暴露访问器而不是让装配处再收一个 `Store` 参数，是为了让"共用一个池"这件事
    /// 无法被调用方搞错。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 插件自注册。
    ///
    /// 永不返回 `Err`：所有失败都以结构化 `rejections` 回给插件方，
    /// 内部错误也归到 `REJECT_CODE_INTERNAL`——插件侧只需要一套处理逻辑。
    ///
    /// 无论接受与否，结果都过一遍 [`Self::note_outcome`]：拒绝要落日志、
    /// 落留痕表（拒绝原因只存在于插件容器日志里的日子，排查「实例 0 个但
    /// 容器活着」得先登上跑插件的机器）；接受则把旧留痕销案。
    pub async fn register(
        &self,
        req: &RegisterRequest,
        source_ip: Option<&str>,
    ) -> RegisterResponse {
        let response = match self.try_register(req, source_ip).await {
            Ok(response) => response,
            Err(err) => {
                tracing::error!(error = %err, plugin = %req.plugin_name, "注册处理内部错误");
                rejected(vec![Rejection {
                    code: RejectCode::Internal as i32,
                    message: "中台内部错误，请稍后重试".to_string(),
                    // 保留原始错误：它只有中台侧看得懂，但正是中台维护方排查所需的线索。
                    // 同时必须点明"不是你的问题"——否则插件方会去反复改自己的配置。
                    detail: format!(
                        "{err}；这不是插件侧的配置问题，重试后仍失败请把这段错误信息交给中台维护方"
                    ),
                }])
            }
        };
        self.note_outcome(req, source_ip, &response).await;
        response
    }

    /// 注册结果的留痕：拒绝 → 日志 + `registration_rejections`；
    /// 接受 → 清掉该实例的旧记录（注册成功即销案）。
    ///
    /// 留痕是 best-effort：写库失败只记 error，绝不反过来影响注册响应本身——
    /// 「看不看得见拒绝原因」是可观测性，「收不收下这个插件」是准入，后者不能被前者拖垮。
    async fn note_outcome(
        &self,
        req: &RegisterRequest,
        source_ip: Option<&str>,
        response: &RegisterResponse,
    ) {
        if response.accepted {
            // 销案按实例走：同一插件的其他实例若有各自的问题，记录还该留着。
            match rejections::clear(self.store.pool(), &req.plugin_name, Some(&req.instance_id))
                .await
            {
                Ok(_) => {}
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        plugin = %req.plugin_name,
                        instance = %req.instance_id,
                        "注册成功后清理拒绝记录失败"
                    );
                }
            }
            return;
        }

        for rejection in &response.rejections {
            tracing::warn!(
                plugin = %req.plugin_name,
                version = %req.version,
                instance = %req.instance_id,
                code = rejection.code,
                message = %rejection.message,
                source_ip = source_ip.unwrap_or("-"),
                "插件注册被拒"
            );
            if let Err(err) = rejections::record(
                self.store.pool(),
                &rejections::NewRejection {
                    plugin_name: &req.plugin_name,
                    instance_id: &req.instance_id,
                    code: rejection.code,
                    version: &req.version,
                    message: &rejection.message,
                    detail: &rejection.detail,
                    source_ip: source_ip.unwrap_or(""),
                },
            )
            .await
            {
                tracing::error!(
                    error = %err,
                    plugin = %req.plugin_name,
                    instance = %req.instance_id,
                    "注册拒绝记录写入失败"
                );
            }
        }
    }

    async fn try_register(
        &self,
        req: &RegisterRequest,
        source_ip: Option<&str>,
    ) -> Result<RegisterResponse> {
        // 1~3) manifest 在不在、descriptor 能不能解析、manifest 自不自洽。
        // 抽在 reject::preflight 里，为的是让 hub-mock（本地替身）跑**同一套**判据。
        let (manifest, index) = match reject::preflight(req) {
            reject::Preflight::Reject(rejections) => return Ok(rejected(rejections)),
            reject::Preflight::Ok { manifest, index } => (manifest, index),
        };

        // 3) 可达性探测。拒绝原因的措辞抽在 reject::probe_rejection 里——
        // hub-mock（本地替身）要回一模一样的话，抄一份就会漂移
        let outcome = self.probe.health(&req.advertise_addr).await;
        if let Some(rejection) = reject::probe_rejection(&req.advertise_addr, &outcome) {
            return Ok(rejected(vec![rejection]));
        }

        // 4) 契约兼容性
        let mut warnings = Vec::new();
        let existing_plugin = plugins::find_plugin(self.store.pool(), &manifest.name).await?;

        if let Some(plugin) = &existing_plugin {
            match plugins::find_version(self.store.pool(), plugin.id, &manifest.version).await? {
                Some(existing) => {
                    // 同版本必须完全一致：版本号是制品身份，同号不同契约会让基线失去意义
                    let stored = ContractIndex::from_descriptor_set(&existing.descriptor)?;
                    if stored != index || existing.manifest != encode_manifest(&manifest) {
                        return Ok(rejected(vec![Rejection {
                            code: RejectCode::VersionConflict as i32,
                            message: format!(
                                "版本 {} 已存在且契约或 manifest 不一致",
                                manifest.version
                            ),
                            detail: "同一版本号不可改变契约；请升版本号后重新注册".to_string(),
                        }]));
                    }
                }
                None => {
                    // 新版本：与最近一次登记的版本比对，拦破坏性变更
                    if let Some(baseline) =
                        plugins::latest_version(self.store.pool(), plugin.id).await?
                    {
                        let base_index = ContractIndex::from_descriptor_set(&baseline.descriptor)?;
                        let changes = check_compatibility(&base_index, &index);
                        if has_breaking(&changes) {
                            return Ok(rejected(vec![Rejection {
                                code: RejectCode::BreakingChange as i32,
                                message: format!(
                                    "相对版本 {} 存在破坏性契约变更",
                                    baseline.version
                                ),
                                // 只把变更列出来不算说完：新版号并不能绕过兼容检查
                                // （同一版本号早就由 VERSION_CONFLICT 拦掉了），所以
                                // 「升版本号」在这里不是出路，得说清唯一的出路是什么。
                                detail: format!(
                                    "{}；基线版本里已有的字段不能删、改类型或改编号，只能新增——\
                                     请把这些字段按原编号原类型改回再注册；若确实要重新设计契约，\
                                     需由中台侧先清掉旧版本基线（管理面删除端点或 hubctl remove-version）",
                                    summarize(&changes).unwrap_or_default()
                                ),
                            }]));
                        }
                        // 非破坏性变更（如字段改名）放行，但要报出来不做静默
                        warnings.extend(
                            changes
                                .iter()
                                .filter(|c| c.severity == Severity::Warning)
                                .map(|c| c.detail.clone()),
                        );
                    }
                }
            }
        }

        // 5) 实例标识校验
        //
        // `instance_id` 完全由插件自己生成、中台此前不做任何校验，但它同时是
        // `plugin_instances` 的唯一键与状态凭证的载体。若放行「B 拿 A 的 instance_id 注册」，
        // upsert 会把那一行的 version_id 改成 B 的并轮换凭证：A 从自己插件的实例列表里
        // 消失（选中它时 NoHealthyInstance），而 A 的心跳仍按 instance_id 命中、
        // 完全察觉不到丢了注册——两边于是每次重注册都互相顶掉，永久空转。
        // 所以这里先查属主，属主是别的插件就拒绝；同一插件（换版本是升级、同版本是重启）
        // 放行，实例行随 upsert 正常迁移。
        // 先跑不需要查库的两条（空、超长），再查属主——属主查询是这条链上唯一一次
        // 额外的数据库往返，能省就省。措辞同样抽在 reject 里给 hub-mock 复用。
        if let Some(rejection) = reject::instance_id_format_rejection(&req.instance_id) {
            return Ok(rejected(vec![rejection]));
        }
        let owner = hub_store::instances::instance_owner(self.store.pool(), &req.instance_id)
            .await?
            .map(|(owner, _version)| owner);
        if let Some(rejection) =
            reject::instance_id_owner_rejection(&req.instance_id, &manifest.name, owner.as_deref())
        {
            return Ok(rejected(vec![rejection]));
        }

        // 6) 落库
        let produces: Vec<String> = manifest
            .produces
            .iter()
            .map(|m| m.fq_name.clone())
            .collect();
        let consumes: Vec<String> = manifest
            .consumes
            .iter()
            .map(|m| m.fq_name.clone())
            .collect();
        let tools: Vec<NewTool<'_>> = manifest
            .tools
            .iter()
            .map(|t| NewTool {
                name: &t.name,
                description: &t.description,
                input_schema_json: &t.input_schema_json,
                requires_approval: t.requires_approval,
            })
            .collect();

        let version = plugins::upsert_version(
            self.store.pool(),
            &NewVersion {
                plugin_name: &manifest.name,
                description: &manifest.description,
                owner: &manifest.owner,
                version: &manifest.version,
                manifest: &encode_manifest(&manifest),
                descriptor: &req.descriptor_set,
                produces: &produces,
                consumes: &consumes,
                tools: &tools,
            },
        )
        .await?;

        let instance = hub_store::instances::upsert_instance(
            self.store.pool(),
            version.row.id,
            &req.instance_id,
            &req.advertise_addr,
            source_ip,
        )
        .await?;

        tracing::info!(
            plugin = %manifest.name,
            version = %manifest.version,
            instance = %instance.instance_id,
            addr = %req.advertise_addr,
            source_ip = source_ip.unwrap_or("-"),
            created_version = version.created,
            "插件已注册"
        );

        // 凭证由 upsert_instance 在库里生成（并随每次注册轮换），这里取回来下发给插件。
        // 取不到说明写入与读取之间出了岔子——宁可拦下来，也不能让插件拿着空凭证去调
        // HubState：那会在每次状态访问时才炸，离病根很远。
        let state_token =
            hub_store::instances::find_state_token(self.store.pool(), &instance.instance_id)
                .await?
                .ok_or_else(|| RegistryError::MissingStateToken(instance.instance_id.clone()))?;

        Ok(RegisterResponse {
            accepted: true,
            instance_id: instance.instance_id,
            heartbeat_interval_seconds: self.cfg.heartbeat_interval_seconds,
            rejections: Vec::new(),
            warnings,
            state_token,
        })
    }

    /// 心跳续期。
    ///
    /// 实例已被摘除时返回 `accepted=false` + `reregister_required=true`：
    /// 这是实例掉线后能自愈的关键，插件 SDK 据此重新走注册流程。
    pub async fn heartbeat(&self, instance_id: &str) -> Result<HeartbeatResponse> {
        let alive =
            hub_store::instances::touch_heartbeat(self.store.pool(), instance_id, Utc::now())
                .await?;

        Ok(HeartbeatResponse {
            accepted: alive,
            heartbeat_interval_seconds: self.cfg.heartbeat_interval_seconds,
            reregister_required: !alive,
        })
    }

    /// 插件主动注销（优雅退出）。
    ///
    /// **要凭注册时下发的状态凭证**（`RegisterResponse.state_token`）认出「你是这一行的
    /// 主人」。`instance_id` 是插件自报的，不同插件之间可以撞（缺省「主机名-PID」，同一
    /// host 网络下容器 PID 又都是 1，必然撞），而只按 `instance_id` 删行会让先退出的一方
    /// 删掉**对方**那一行——对方的心跳仍按 `instance_id` 命中、返回 accepted，完全察觉
    /// 不到自己从注册表里消失了。
    ///
    /// 凭证不符或为空一律**拒绝并告警、不删任何行**。代价只是优雅退出退化成「等心跳超时
    /// 被摘除」（[`Self::sweep_stale`] 兜住，只是晚一点），而放行的代价是可能删掉别的
    /// 插件的实例行。这正是中台既有的立场：不信插件自报的身份，只信注册时下发的凭证
    /// （`hub_store::instances::plugin_of_state_token` 同此）。
    pub async fn unregister(
        &self,
        instance_id: &str,
        state_token: &str,
        reason: &str,
    ) -> Result<bool> {
        if state_token.is_empty() {
            tracing::warn!(
                instance = %instance_id,
                reason,
                "注销请求没带状态凭证，已拒绝——没注册成功就没有凭证，\
                 也就注销不了；实例会由心跳超时摘除"
            );
            return Ok(false);
        }

        let removed = hub_store::instances::delete_instance_with_token(
            self.store.pool(),
            instance_id,
            state_token,
        )
        .await?;

        if removed {
            tracing::info!(instance = %instance_id, reason, "插件主动注销");
            return Ok(true);
        }

        // 没删掉只有两种可能：这一行已经不在了，或凭证对不上。**只影响日志措辞**——
        // 判定已由上面那条带条件的 DELETE 做完，这里查出来的东西不用来放行。
        //
        // `find_state_token` 返回 `None` 也有两种含义：行不在了，**或者它的凭证本就是
        // 空的**（迁移前登记的旧行）。日志要如实说，别把后者说成「不存在」。
        match hub_store::instances::find_state_token(self.store.pool(), instance_id).await? {
            None => tracing::info!(
                instance = %instance_id,
                reason,
                "注销未摘除：该实例行不存在，或其凭证为空（迁移前登记的旧行）"
            ),
            Some(_) => tracing::warn!(
                instance = %instance_id,
                reason,
                "注销被拒：凭证与注册时下发的不符（已被重新注册轮换，或本就来自别的插件）"
            ),
        }
        Ok(false)
    }

    /// 摘除心跳超时的实例。由后台任务周期性调用。
    pub async fn sweep_stale(&self) -> Result<Vec<String>> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(self.cfg.stale_after)
                .unwrap_or_else(|_| chrono::Duration::seconds(30));
        let removed = hub_store::instances::sweep_stale(self.store.pool(), cutoff).await?;
        if !removed.is_empty() {
            tracing::warn!(count = removed.len(), instances = ?removed, "摘除心跳超时的实例");
        }
        Ok(removed)
    }

    /// 为一次调用选择实例。
    ///
    /// `version` 为 `None` 表示取该插件最近的版本——M1 只做「指定版本 / 最新版本」，
    /// 语义化版本约束（如 `^2.1`）随 flow 定义在 M2 引入。
    pub async fn resolve(
        &self,
        plugin_name: &str,
        version: Option<&str>,
    ) -> Result<InstanceTarget> {
        let plugin = plugins::find_plugin(self.store.pool(), plugin_name)
            .await?
            .ok_or_else(|| RegistryError::PluginNotFound(plugin_name.to_string()))?;

        let version_row = match version {
            Some(v) => plugins::find_version(self.store.pool(), plugin.id, v)
                .await?
                .ok_or_else(|| RegistryError::VersionNotFound {
                    plugin: plugin_name.to_string(),
                    version: v.to_string(),
                })?,
            None => plugins::latest_version(self.store.pool(), plugin.id)
                .await?
                .ok_or_else(|| RegistryError::VersionNotFound {
                    plugin: plugin_name.to_string(),
                    version: "latest".to_string(),
                })?,
        };

        let instances =
            hub_store::instances::instances_of_version(self.store.pool(), version_row.id).await?;

        // 多个副本时取第一个健康的。M1 不做负载均衡策略，先保证「有可用实例」。
        let chosen = instances
            .into_iter()
            .find(|i| i.status == "healthy")
            .ok_or_else(|| RegistryError::NoHealthyInstance {
                plugin: plugin_name.to_string(),
            })?;

        Ok(InstanceTarget {
            plugin_name: plugin.name,
            version: version_row.version,
            instance_id: chosen.instance_id,
            advertise_addr: chosen.advertise_addr,
        })
    }
}

fn rejected(rejections: Vec<Rejection>) -> RegisterResponse {
    RegisterResponse {
        accepted: false,
        instance_id: String::new(),
        heartbeat_interval_seconds: 0,
        rejections,
        warnings: Vec::new(),
        // 注册被拒，不下发凭证：拒绝分支在 upsert_instance 之前就返回，
        // 库里根本没生成过凭证，也不该给未通过校验的实例发身份
        state_token: String::new(),
    }
}

/// manifest 落库前统一编码，保证「同版本必须字节一致」这条判断可用。
fn encode_manifest(manifest: &hub_proto::v1::PluginManifest) -> Vec<u8> {
    use prost::Message as _;
    manifest.encode_to_vec()
}
