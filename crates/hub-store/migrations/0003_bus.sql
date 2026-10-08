-- M3 异步与总线：死信、幂等、触发器、超限载荷的引用通道
--
-- 这一批表的共同前提是：**Stream 的投递语义是 at-least-once**。「不丢消息」的反面
-- 就是「会重投」，所以幂等不是可选项而是地基——没有它，重投会把一次执行放大成多次。

-- ---------------------------------------------------------------------------
-- 幂等
-- ---------------------------------------------------------------------------

-- 一次「本该只发生一次」的事件的去重记录。
--
-- 键的构造由调用方决定（节点执行用 `run_id:node_id`，外部投递用业务单号），
-- 这里只负责「第一次见到返回 true、之后再见到返回 false」。
--
-- 带 expires_at 而不是永久保留：表会随时间无限长，而幂等的价值只覆盖消息可能被
-- 重投的那个时间窗（默认 24h，与 STREAM_RETENTION_HOURS 对齐）。
CREATE TABLE idempotency_keys (
    key        TEXT        NOT NULL PRIMARY KEY,

    -- 这次事件落在了哪次执行上，排障时用
    run_id     TEXT,

    seen_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idempotency_keys_expiry_idx ON idempotency_keys (expires_at);

-- ---------------------------------------------------------------------------
-- 死信
-- ---------------------------------------------------------------------------

-- 重投耗尽后的消息。
--
-- **进死信而不是丢弃**：一条消息反复失败说明它不是暂时性问题，继续重投只是持续
-- 消耗资源；而直接丢掉会让「这条数据去哪了」变成一个查不到答案的问题。
--
-- payload 存摘要而不是全量：与 run/span 的留存策略一致（全量落库的存储与合规
-- 代价都受不了）。真要重放时，重放用的载荷来自 `payload_ref` 指向的引用通道或
-- 原始请求的调用方，不来自这张表。
CREATE TABLE dead_letters (
    id           BIGSERIAL   PRIMARY KEY,

    -- 来自哪条 Stream 的哪个消息 id（Redis 的 `<ms>-<seq>`）
    stream       TEXT        NOT NULL,
    stream_id    TEXT        NOT NULL,

    -- 归属：哪条 flow 的哪个节点
    run_id       TEXT,
    flow_name    TEXT,
    node_id      TEXT,

    -- 尝试了多少次才放弃，以及最后一次的错误
    attempts     INTEGER     NOT NULL DEFAULT 0,
    error        TEXT        NOT NULL DEFAULT '',

    -- 载荷摘要（类型、字节数、关键 meta），不落全量
    payload_summary TEXT,

    -- 重放：留下「重放成了哪次新执行」的链路，否则死信重放后就没法追踪了
    replayed_run_id TEXT,
    replayed_at     TIMESTAMPTZ,

    first_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 同一个消息只该有一条死信记录：重投耗尽后重复进死信应当无害
CREATE UNIQUE INDEX dead_letters_stream_msg ON dead_letters (stream, stream_id);
CREATE INDEX dead_letters_pending_idx ON dead_letters (last_attempt_at DESC)
    WHERE replayed_at IS NULL;
CREATE INDEX dead_letters_run_idx ON dead_letters (run_id);

-- ---------------------------------------------------------------------------
-- 触发器
-- ---------------------------------------------------------------------------

-- 统一触发模型：同一条 flow 可以由多种方式触发。
--
-- HTTP 触发不需要建行（路由 /flows/{name}/trigger 天然存在）；这张表管的是
-- 「需要常驻监听才有意义」的那些：cron 定时与 MQ 订阅。
CREATE TABLE triggers (
    id         BIGSERIAL PRIMARY KEY,
    flow_id    BIGINT    NOT NULL REFERENCES flows (id) ON DELETE CASCADE,

    -- cron: 表达式在 config_json.expr；mq: 订阅的 stream 在 config_json.stream
    kind       TEXT      NOT NULL CHECK (kind IN ('cron', 'mq')),

    -- 同一条 flow 下同类触发器靠它区分（多条 cron 各跑各的）
    name       TEXT      NOT NULL DEFAULT 'default',

    -- 触发时套用的信封模板：载荷从哪取、meta 怎么补、超时给多少
    config     JSONB     NOT NULL DEFAULT '{}'::jsonb,

    enabled    BOOLEAN   NOT NULL DEFAULT true,

    -- 上一次触发的时间与结果，控制台直接读它回答「这个定时器还活着吗」
    last_fired_at   TIMESTAMPTZ,
    last_error      TEXT,
    fired_count     BIGINT  NOT NULL DEFAULT 0,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (flow_id, kind, name)
);

CREATE INDEX triggers_enabled_idx ON triggers (kind) WHERE enabled;

-- ---------------------------------------------------------------------------
-- 超限载荷的引用通道
-- ---------------------------------------------------------------------------

-- 内联放不下（>4MB）的载荷落到这里，信封里只留一个引用。
--
-- 存 PG 的 BYTEA 而不是对象存储：UAT 没有对象存储，为这一步单独引一套服务不划算；
-- 而这类载荷本来就是**中转**而不是归档——带 TTL，默认 1 小时，过期即清理。
-- 「不会长期保存」是设计的一部分，不是妥协。
CREATE TABLE payload_blobs (
    id         TEXT        PRIMARY KEY,
    size       BIGINT      NOT NULL,
    sha256     TEXT        NOT NULL,

    -- 原始字节。压缩与否由写入方决定，这里保持原样以便逐字节还原
    bytes      BYTEA       NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX payload_blobs_expiry_idx ON payload_blobs (expires_at);

-- ---------------------------------------------------------------------------
-- 异步执行的 run 状态
-- ---------------------------------------------------------------------------

-- 异步触发时，`runs` 行在**入队那一刻**就写下来（状态 queued），而不是等 worker
-- 捡起来才写。
--
-- 这个顺序是刻意的：入队成功却查不到 run，调用方会以为触发丢了；先落行再投递，
-- 「投递失败」就变成一个可补偿的状态（重投或标失败），而不是一条无迹可寻的消息。
ALTER TABLE runs DROP CONSTRAINT IF EXISTS runs_status_check;
ALTER TABLE runs ADD CONSTRAINT runs_status_check
    CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'rejected'));

-- 排队中的执行：控制台与「堆积降级」都要按它回答「现在堵了多少」
CREATE INDEX runs_queued_idx ON runs (started_at) WHERE status = 'queued';
