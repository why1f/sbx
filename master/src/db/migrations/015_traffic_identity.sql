-- 新用户的记账身份独立于可复用的显示名。NULL 保留老用户原有计数键,
-- 因此升级不切换已有流量的基线;删除后重建则由 add_user 分配新身份。
ALTER TABLE users ADD COLUMN traffic_name TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_traffic_name ON users(COALESCE(traffic_name, name));
-- 让升级前可能遗漏的禁用名单重新同步一次。
UPDATE agents SET user_state_revision = user_state_revision + 1;
