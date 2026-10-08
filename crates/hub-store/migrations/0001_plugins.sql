-- M1 数据模型：插件定义 / 版本 / 契约 / 工具 / 实例
--
-- 版本与实例分开是刻意的：同一插件的多个版本可以同时在线（flow 按版本约束选实例，
-- 天然支持灰度），而同一版本又可以有多个副本。摘除实例不影响插件定义。

-- 逻辑插件。同名多版本共存。
CREATE TABLE plugins (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT        NOT NULL UNIQUE,
    description TEXT        NOT NULL DEFAULT '',
    owner       TEXT        NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 插件版本。
--
-- manifest 存 proto 编码的 PluginManifest（而非 JSON）：prost 没有原生的
-- protobuf↔JSON 映射，转 JSON 要额外引 pbjson；proto 字节无损且更省。
-- 需要按内容查询的部分（消费/生产的消息类型、MCP 工具）拆到了下面两张表。
--
-- descriptor 是该版本注册时提交的 FileDescriptorSet，作为后续版本的兼容性基线。
CREATE TABLE plugin_versions (
    id         BIGSERIAL PRIMARY KEY,
    plugin_id  BIGINT      NOT NULL REFERENCES plugins (id) ON DELETE CASCADE,
    version    TEXT        NOT NULL,
    manifest   BYTEA       NOT NULL,
    descriptor BYTEA       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, version)
);

-- 版本声明的消息类型（契约）。
--
-- 独立成表是为了回答「谁生产/消费了 wms.v1.OrderCreated」——改契约前的影响面分析、
-- 编排时的上下游匹配都要用它，从 manifest 里现解析太慢。
CREATE TABLE plugin_contracts (
    version_id BIGINT NOT NULL REFERENCES plugin_versions (id) ON DELETE CASCADE,
    direction  TEXT   NOT NULL CHECK (direction IN ('produces', 'consumes')),
    fq_name    TEXT   NOT NULL,
    PRIMARY KEY (version_id, direction, fq_name)
);

CREATE INDEX plugin_contracts_fq_name_idx ON plugin_contracts (fq_name);

-- 版本声明的 MCP 工具。
--
-- 独立成表是为了给 MCP 工具面直接出列表，不必每次把 manifest 全解一遍。
-- 注意：工具名在中台聚合时会加 `插件名__` 前缀，跨插件重名不会冲突；
-- 需要拦的是同一插件版本内声明重复的工具名，那属于 manifest 校验，不在表约束里。
CREATE TABLE plugin_tools (
    version_id        BIGINT  NOT NULL REFERENCES plugin_versions (id) ON DELETE CASCADE,
    name              TEXT    NOT NULL,
    description       TEXT    NOT NULL DEFAULT '',
    input_schema_json TEXT    NOT NULL DEFAULT '',
    requires_approval BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (version_id, name)
);

-- 实例。一个版本可以有多个副本；心跳超时摘除实例，但保留插件定义。
CREATE TABLE plugin_instances (
    id                BIGSERIAL PRIMARY KEY,
    version_id        BIGINT      NOT NULL REFERENCES plugin_versions (id) ON DELETE CASCADE,
    instance_id       TEXT        NOT NULL UNIQUE,
    advertise_addr    TEXT        NOT NULL,
    status            TEXT        NOT NULL DEFAULT 'healthy',
    -- 注册来源 IP。插件面不鉴权（设计决策），审计里至少要能追到来源。
    source_ip         TEXT,
    registered_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX plugin_instances_version_idx ON plugin_instances (version_id);
CREATE INDEX plugin_instances_heartbeat_idx ON plugin_instances (last_heartbeat_at);
