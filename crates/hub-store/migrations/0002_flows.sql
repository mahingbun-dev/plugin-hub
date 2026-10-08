-- M2 编排：flow 定义、修订版本与执行记录
--
-- 「草稿 / 已发布」是一等状态而不是布尔标记：编排决定生产流量走向，
-- 改动必须先落成草稿、经人审批后才发布（agent 只能改草稿，见 docs/design.md）。
-- 用 status + 部分唯一索引来保证「一条 flow 同时只有一份草稿」。

CREATE TABLE flows (
    id                 BIGSERIAL PRIMARY KEY,
    name               TEXT        NOT NULL UNIQUE,
    description        TEXT        NOT NULL DEFAULT '',

    -- 当前已发布的修订号；0 表示从未发布过
    published_revision INTEGER     NOT NULL DEFAULT 0,

    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 每一次修订。草稿与历史发布版本都在这里，靠 status 区分。
CREATE TABLE flow_revisions (
    id           BIGSERIAL PRIMARY KEY,
    flow_id      BIGINT      NOT NULL REFERENCES flows (id) ON DELETE CASCADE,
    revision     INTEGER     NOT NULL,

    -- 编排定义（hub-flow 的 FlowDefinition 的 JSON 形态）
    definition   JSONB       NOT NULL,

    status       TEXT        NOT NULL CHECK (status IN ('draft', 'published', 'archived')),

    -- 保存时的校验结果快照。排障时最常问的是「这条当时为什么存不下去」，
    -- 留着它就不用去翻日志。
    validation   JSONB,

    created_by   TEXT        NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at TIMESTAMPTZ,

    UNIQUE (flow_id, revision)
);

-- 一条 flow 同时只能有一份草稿
CREATE UNIQUE INDEX flow_revisions_one_draft
    ON flow_revisions (flow_id) WHERE status = 'draft';

-- 一次 flow 执行。
CREATE TABLE runs (
    -- 执行 id（ULID），对外暴露、进 trace，是排障时的一等公民
    run_id        TEXT        NOT NULL PRIMARY KEY,
    flow_id       BIGINT      NOT NULL REFERENCES flows (id) ON DELETE CASCADE,
    flow_revision INTEGER     NOT NULL,

    -- W3C Trace Context，贯穿中台与所有插件
    trace_id      TEXT        NOT NULL,

    -- 调用主体（谁触发的）与触发来源（怎么触发的）
    subject       JSONB,
    trigger       JSONB,

    status        TEXT        NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'rejected')),

    -- 入参摘要。**不落全量报文**——存储与合规都受不了（见 docs/design.md 的留存策略）
    input_summary  TEXT,

    error         TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ
);

CREATE INDEX runs_started_idx ON runs (started_at DESC);
CREATE INDEX runs_trace_idx ON runs (trace_id);
CREATE INDEX runs_flow_idx ON runs (flow_id, started_at DESC);

-- 一次执行里的单个节点。
--
-- 这张表是「这条数据卡在哪一跳」的答案：谁、跑了多久、第几次尝试、成败与原因。
CREATE TABLE run_nodes (
    id          BIGSERIAL PRIMARY KEY,
    run_id      TEXT        NOT NULL REFERENCES runs (run_id) ON DELETE CASCADE,
    node_id     TEXT        NOT NULL,
    plugin      TEXT        NOT NULL,
    version     TEXT        NOT NULL,
    instance_id TEXT,

    -- 第几次尝试（从 1 开始）。重试会留下多行，便于看出「重试了几次才好」
    attempt     INTEGER     NOT NULL DEFAULT 1,

    status      TEXT        NOT NULL CHECK (status IN ('succeeded', 'failed', 'rejected', 'skipped')),

    duration_ms BIGINT,
    io_summary   TEXT,
    error       TEXT,

    started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,

    -- 同一次执行里的同一个节点的同一次尝试只该有一条记录，重复写入应当无害
    UNIQUE (run_id, node_id, attempt)
);

CREATE INDEX run_nodes_run_idx ON run_nodes (run_id);

-- 调用链 span。
--
-- 自存而不是只依赖 OTel 后端：控制台要能按 traceId 直接查到「这条数据经过了哪些插件、
-- 在哪一跳慢了」，采样后的 trace 后端给不了这个确定性。按天清理，默认留 7 天。
CREATE TABLE spans (
    id             BIGSERIAL PRIMARY KEY,
    trace_id       TEXT        NOT NULL,
    span_id        TEXT        NOT NULL,
    parent_span_id TEXT,

    run_id         TEXT,
    node_id        TEXT,
    name           TEXT        NOT NULL,

    started_at     TIMESTAMPTZ NOT NULL,
    duration_ms    BIGINT      NOT NULL,
    status         TEXT        NOT NULL,

    attributes     JSONB,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX spans_trace_idx ON spans (trace_id, started_at);
CREATE INDEX spans_created_idx ON spans (created_at);
