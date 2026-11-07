-- 每个接收方只保留当前成功通知档位,失败不会抑制其他接收方的重试。
CREATE TABLE IF NOT EXISTS tg_quota_deliveries (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    chat_id INTEGER NOT NULL,
    level INTEGER NOT NULL,
    retry_at INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, chat_id)
);
CREATE INDEX IF NOT EXISTS idx_agent_commands_inflight ON agent_commands(taken_at) WHERE done_at IS NULL;
