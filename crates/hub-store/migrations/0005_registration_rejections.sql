-- 注册拒绝的留痕表。
--
-- 注册被拒的原因此前只存在于插件容器的日志里：容器活着、每 5 秒重试一次、
-- 中台每轮都拒——而控制台与 /admin 面上只能看到「实例 0 个」，看不出为什么
-- （实例掉线会被 sweep_stale 直接 DELETE，问题根本不在实例表里）。
-- 这张表把「最近一次被拒」变成可查询的中台侧事实，管理面据此展示。
--
-- 为什么 upsert 而不是逐次 INSERT：被拒的插件会按自己的节奏（实测 5 秒）
-- 无限重试，逐次插入会以每插件每 5 秒一行的速度膨胀；这里按
-- (plugin_name, instance_id, code) 收敛成一行，count 累计重试次数。
-- 同键不同 version 属于「同一件事的最新一次尝试」：覆盖 version / message / detail，
-- first_seen_at 保持不动——「这个问题存在多久了」比「重试了多少轮」更重要。
--
-- code 用 hub.v1.RejectCode 的 i32 值：拒绝码是 proto enum，存数字而非文本，
-- 展示层的名字映射由调用方做（枚举改名不会让历史数据失义）。

CREATE TABLE registration_rejections (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    plugin_name TEXT NOT NULL,
    instance_id TEXT NOT NULL DEFAULT '',
    code INT NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    source_ip TEXT NOT NULL DEFAULT '',
    count BIGINT NOT NULL DEFAULT 1,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (plugin_name, instance_id, code)
);

-- 列表页按「最近还在被拒」排序；巡检清理（30 天前）也走这个索引。
CREATE INDEX registration_rejections_last_seen_idx
    ON registration_rejections (last_seen_at DESC);
