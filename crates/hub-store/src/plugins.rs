//! 插件、版本与其契约 / 工具的读写。

use sqlx::PgPool;

use crate::Result;
use crate::model::{
    ContractRow, OnlineToolRow, PluginOverviewRow, PluginRow, PluginVersionContractsRow,
    PluginVersionRow, ToolRow,
};

/// 新建一个版本所需的全部输入（来自插件的注册请求）。
pub struct NewVersion<'a> {
    pub plugin_name: &'a str,
    pub description: &'a str,
    pub owner: &'a str,
    pub version: &'a str,

    /// proto 编码的 `hub.v1.PluginManifest`
    pub manifest: &'a [u8],

    /// 注册时提交的 `FileDescriptorSet`
    pub descriptor: &'a [u8],

    pub produces: &'a [String],
    pub consumes: &'a [String],
    pub tools: &'a [NewTool<'a>],
}

#[derive(Debug, Clone)]
pub struct NewTool<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub input_schema_json: &'a str,
    pub requires_approval: bool,
}

/// 版本写入结果。
#[derive(Debug, Clone)]
pub struct VersionUpsert {
    pub row: PluginVersionRow,

    /// true 表示本次新建了版本；false 表示该版本早已存在（契约一致性由调用方比对）。
    pub created: bool,
}

/// 插件详情（含版本列表）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginDetail {
    pub plugin: PluginRow,
    pub versions: Vec<PluginVersionRow>,
}

pub async fn find_plugin(pool: &PgPool, name: &str) -> Result<Option<PluginRow>> {
    let row = sqlx::query_as::<_, PluginRow>(
        "SELECT id, name, description, owner, created_at, updated_at FROM plugins WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_version(
    pool: &PgPool,
    plugin_id: i64,
    version: &str,
) -> Result<Option<PluginVersionRow>> {
    let row = sqlx::query_as::<_, PluginVersionRow>(
        "SELECT id, plugin_id, version, manifest, descriptor, created_at
           FROM plugin_versions WHERE plugin_id = $1 AND version = $2",
    )
    .bind(plugin_id)
    .bind(version)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 该插件最近一次登记的版本，作为新版本的兼容性基线。
pub async fn latest_version(pool: &PgPool, plugin_id: i64) -> Result<Option<PluginVersionRow>> {
    let row = sqlx::query_as::<_, PluginVersionRow>(
        "SELECT id, plugin_id, version, manifest, descriptor, created_at
           FROM plugin_versions WHERE plugin_id = $1
          ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(plugin_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// 取某个版本的 manifest 原始字节（proto 编码的 `hub.v1.PluginManifest`）。
///
/// 契约与工具已有结构化查询（[`contracts_of`]/[`tools_of`]），这条专门服务
/// 「要从 manifest 里解出其它字段」的调用方——目前是插件互调的权限校验：
/// 调用方声明了哪些可调用的目标插件（`invokes`），只存在于 manifest 字节里，
/// 没有也不值得为它单独立表。版本不存在返回 `None`。
pub async fn manifest_of_version(pool: &PgPool, version_id: i64) -> Result<Option<Vec<u8>>> {
    let row = sqlx::query_as::<_, (Vec<u8>,)>("SELECT manifest FROM plugin_versions WHERE id = $1")
        .bind(version_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(manifest,)| manifest))
}

/// 写入插件与版本（同一事务）。
///
/// 版本已存在时不覆盖——契约一致性由调用方比对后决定是否放行，这里不做静默改写。
pub async fn upsert_version(pool: &PgPool, input: &NewVersion<'_>) -> Result<VersionUpsert> {
    let mut tx = pool.begin().await?;

    let plugin = sqlx::query_as::<_, PluginRow>(
        "INSERT INTO plugins (name, description, owner) VALUES ($1, $2, $3)
         ON CONFLICT (name) DO UPDATE
            SET description = EXCLUDED.description,
                owner = EXCLUDED.owner,
                updated_at = now()
         RETURNING id, name, description, owner, created_at, updated_at",
    )
    .bind(input.plugin_name)
    .bind(input.description)
    .bind(input.owner)
    .fetch_one(&mut *tx)
    .await?;

    let inserted = sqlx::query_as::<_, PluginVersionRow>(
        "INSERT INTO plugin_versions (plugin_id, version, manifest, descriptor)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (plugin_id, version) DO NOTHING
         RETURNING id, plugin_id, version, manifest, descriptor, created_at",
    )
    .bind(plugin.id)
    .bind(input.version)
    .bind(input.manifest)
    .bind(input.descriptor)
    .fetch_optional(&mut *tx)
    .await?;

    let (row, created) = match inserted {
        Some(row) => (row, true),
        None => (
            sqlx::query_as::<_, PluginVersionRow>(
                "SELECT id, plugin_id, version, manifest, descriptor, created_at
                   FROM plugin_versions WHERE plugin_id = $1 AND version = $2",
            )
            .bind(plugin.id)
            .bind(input.version)
            .fetch_one(&mut *tx)
            .await?,
            false,
        ),
    };

    // 契约与工具只在新建版本时写入：已存在的版本其内容必然与首次写入一致
    // （不一致的情况调用方会先行拒绝），重复插入没有意义。
    if created {
        sqlx::query(
            "INSERT INTO plugin_contracts (version_id, direction, fq_name)
             SELECT $1, $2, fq FROM UNNEST($3::text[]) AS t(fq)",
        )
        .bind(row.id)
        .bind(ContractRow::PRODUCES)
        .bind(input.produces)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO plugin_contracts (version_id, direction, fq_name)
             SELECT $1, $2, fq FROM UNNEST($3::text[]) AS t(fq)",
        )
        .bind(row.id)
        .bind(ContractRow::CONSUMES)
        .bind(input.consumes)
        .execute(&mut *tx)
        .await?;

        let tool_names: Vec<String> = input.tools.iter().map(|t| t.name.to_string()).collect();
        let tool_descs: Vec<String> = input
            .tools
            .iter()
            .map(|t| t.description.to_string())
            .collect();
        let tool_schemas: Vec<String> = input
            .tools
            .iter()
            .map(|t| t.input_schema_json.to_string())
            .collect();
        let tool_approvals: Vec<bool> = input.tools.iter().map(|t| t.requires_approval).collect();

        sqlx::query(
            "INSERT INTO plugin_tools (version_id, name, description, input_schema_json, requires_approval)
             SELECT $1, name, description, input_schema_json, requires_approval
               FROM UNNEST($2::text[], $3::text[], $4::text[], $5::bool[])
                    AS t(name, description, input_schema_json, requires_approval)",
        )
        .bind(row.id)
        .bind(&tool_names)
        .bind(&tool_descs)
        .bind(&tool_schemas)
        .bind(&tool_approvals)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(VersionUpsert { row, created })
}

/// 插件总览列表（控制台列表页）。
pub async fn list_plugins(pool: &PgPool) -> Result<Vec<PluginOverviewRow>> {
    let rows = sqlx::query_as::<_, PluginOverviewRow>(
        "SELECT p.id,
                p.name,
                p.description,
                p.owner,
                (SELECT count(*) FROM plugin_versions v WHERE v.plugin_id = p.id) AS version_count,
                (SELECT count(*)
                   FROM plugin_instances i
                   JOIN plugin_versions v ON v.id = i.version_id
                  WHERE v.plugin_id = p.id AND i.status = 'healthy') AS instance_count
           FROM plugins p
          ORDER BY p.name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn describe_plugin(pool: &PgPool, name: &str) -> Result<Option<PluginDetail>> {
    let Some(plugin) = find_plugin(pool, name).await? else {
        return Ok(None);
    };
    let versions = sqlx::query_as::<_, PluginVersionRow>(
        "SELECT id, plugin_id, version, manifest, descriptor, created_at
           FROM plugin_versions WHERE plugin_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(plugin.id)
    .fetch_all(pool)
    .await?;
    Ok(Some(PluginDetail { plugin, versions }))
}

pub async fn contracts_of(pool: &PgPool, version_id: i64) -> Result<Vec<ContractRow>> {
    let rows = sqlx::query_as::<_, ContractRow>(
        "SELECT version_id, direction, fq_name FROM plugin_contracts
          WHERE version_id = $1 ORDER BY direction, fq_name",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn tools_of(pool: &PgPool, version_id: i64) -> Result<Vec<ToolRow>> {
    let rows = sqlx::query_as::<_, ToolRow>(
        "SELECT version_id, name, description, input_schema_json, requires_approval
           FROM plugin_tools WHERE version_id = $1 ORDER BY name",
    )
    .bind(version_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// **此刻真的能调**的插件工具，供 MCP 工具面聚合。
///
/// 每个插件**只出最新登记版本的工具，且该版本必须有在线实例行**（实例行被摘除即
/// 下线，见 `instances::sweep_stale`）。两条限制合起来与调用路由保持严格一致：
/// `Registry::select_instance` 不指定版本时永远路由到最新登记版本、不回退旧版本——
/// 面上下发的每个工具，调用一定打到同版本的实例；面上下发不了的（最新版没实例），
/// 调用也必然 `NoHealthyInstance`。旧版本哪怕还有实例在跑，它独有的工具也不下发：
/// 路由送不到，暴露了只是「看得见调不着」的错位。
///
/// 工具名要拼成 MCP 的 `插件名__工具名`；同版本内的工具重名在注册时就被
/// `hub-registry` 的 `validate_tools` 拦住，这里无需再去重。
pub async fn online_tools(pool: &PgPool) -> Result<Vec<OnlineToolRow>> {
    let rows = sqlx::query_as::<_, OnlineToolRow>(
        "WITH latest AS (
           SELECT v.plugin_id, max(v.id) AS version_id
             FROM plugin_versions v
            GROUP BY v.plugin_id
         )
         SELECT p.name AS plugin_name, t.name, t.description,
                t.input_schema_json, t.requires_approval
           FROM latest l
           JOIN plugin_versions v ON v.id = l.version_id
           JOIN plugins p ON p.id = v.plugin_id
           JOIN plugin_tools t ON t.version_id = v.id
          WHERE EXISTS (SELECT 1 FROM plugin_instances i WHERE i.version_id = v.id)
          ORDER BY p.name, t.name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_plugin(pool: &PgPool, name: &str) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM plugins WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// 删除某个版本（连同它的契约、工具与实例，由外键级联）。
///
/// 插件版本是制品身份，删掉它属于不可逆的运维处置，只服务于一条恢复路径：
/// 注册被 VERSION_CONFLICT / BREAKING_CHANGE 拒死后，删掉旧登记让插件重新注册。
/// 两个入口共用这里：管理面 `DELETE /admin/plugins/{name}/versions/{version}`
/// （authz 的 SCOPE_ADMIN 守卫）与主机面 `hubctl remove-version`（socket 权限守卫）——
/// 各自的守卫都在各自的面上，store 层不做二次校验。
pub async fn delete_version(pool: &PgPool, plugin_name: &str, version: &str) -> Result<bool> {
    let affected = sqlx::query(
        "DELETE FROM plugin_versions
          WHERE version = $2
            AND plugin_id = (SELECT id FROM plugins WHERE name = $1)",
    )
    .bind(plugin_name)
    .bind(version)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// 每个插件每个版本的契约聚合，供编排校验使用。
///
/// 结果按 `(插件名, 新到旧)` 排序——编排校验器据此把「取最新版本」解析成具体版本，
/// 顺序错了会挑到旧版本。
pub async fn versions_with_contracts(pool: &PgPool) -> Result<Vec<PluginVersionContractsRow>> {
    let rows = sqlx::query_as::<_, PluginVersionContractsRow>(
        "SELECT p.name AS plugin,
                v.version,
                COALESCE(array_agg(DISTINCT c.fq_name) FILTER (WHERE c.direction = 'produces'), '{}') AS produces,
                COALESCE(array_agg(DISTINCT c.fq_name) FILTER (WHERE c.direction = 'consumes'), '{}') AS consumes
           FROM plugins p
           JOIN plugin_versions v ON v.plugin_id = p.id
           LEFT JOIN plugin_contracts c ON c.version_id = v.id
          GROUP BY p.name, v.version, v.created_at, v.id
          ORDER BY p.name, v.created_at DESC, v.id DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 谁生产 / 消费了某个消息类型。改契约前的影响面分析、编排时的连线候选都靠它。
pub async fn versions_by_fq_name(
    pool: &PgPool,
    fq_name: &str,
    direction: &str,
) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT p.name, v.version
           FROM plugin_contracts c
           JOIN plugin_versions v ON v.id = c.version_id
           JOIN plugins p ON p.id = v.plugin_id
          WHERE c.fq_name = $1 AND c.direction = $2
          ORDER BY p.name, v.version",
    )
    .bind(fq_name)
    .bind(direction)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
