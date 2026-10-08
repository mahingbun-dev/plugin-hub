-- 外置状态（HubState）的实例凭证。
--
-- 由数据库生成而不是应用侧：gen_random_uuid() 是 PG 13+ 的内置函数（不需要
-- pgcrypto），122 位随机性足够做凭证，且省掉一个 CSPRNG 依赖——中台是离线构建，
-- 每多一个依赖都要重做 vendor 快照。
--
-- DEFAULT 让迁移前登记的旧行有一个确定的值（空串 = 无凭证），
-- 不会因为加 NOT NULL 列而失败。
ALTER TABLE plugin_instances
    ADD COLUMN state_token TEXT NOT NULL DEFAULT '';

-- 凭证反查插件是 HubState 每次调用的必经路径。
CREATE INDEX plugin_instances_state_token_idx
    ON plugin_instances (state_token)
    WHERE state_token <> '';
