-- sbx 初始 schema(DESIGN.md §6.1)
--
-- 与旧项目 singbox-manager 的 001_init.sql 的关键区别:
--   * 一切以 agent 为单位分区:nodes 有 agent_id,新增 agents 表
--   * user_traffic 主键是 (user_id, node_id) —— 一个用户可跨 agent 拥有多个节点(§0.0-3)
--   * 网卡流量(agent_nic_traffic)与用户流量是**两套独立的数字**(§6.4 / §7.2)

CREATE TABLE agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  -- sha256(token) 十六进制。明文只在生成时显示一次(§8.1)。
  -- 不用 argon2:token 是 32 字节 OsRng,慢 KDF 换不到安全性却送出一个 DoS 面(§8.1)。
  token_hash TEXT NOT NULL,
  -- 前 8 位。既给人识别,也让校验能 O(1) 定位到候选行。
  token_prefix TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'never',   -- never|online|offline
  last_seen INTEGER,
  agent_version TEXT,
  singbox_version TEXT,
  os TEXT,
  arch TEXT,
  hostname TEXT,
  ipv4 TEXT,                         -- agent 自探上报,主控可手工覆盖(§7.3)
  ipv6 TEXT,
  nic_quota_bytes INTEGER,           -- NULL = 不限流量
  nic_reset_day INTEGER,             -- NULL = 无需重置;否则 1..31
  -- 两个 revision **独立递增**。合并会造成:离线期间的禁用永久失效,
  -- 或高频禁用路径退化成全量 box 重建(§4.1)。
  config_revision INTEGER NOT NULL DEFAULT 0,
  user_state_revision INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_agents_token_prefix ON agents(token_prefix);

CREATE TABLE agent_nic_traffic (
  agent_id INTEGER PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
  boot_id TEXT,
  last_rx INTEGER NOT NULL DEFAULT 0,
  last_tx INTEGER NOT NULL DEFAULT 0,
  cycle_rx INTEGER NOT NULL DEFAULT 0,
  cycle_tx INTEGER NOT NULL DEFAULT 0,
  cycle_start INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE nodes (
  id INTEGER PRIMARY KEY,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  tag TEXT NOT NULL,
  protocol TEXT NOT NULL,
  listen_port INTEGER NOT NULL,
  params_json TEXT NOT NULL,         -- server_name / path / port_reuse / ipv6 / relay
  UNIQUE(agent_id, tag)
);
CREATE INDEX idx_nodes_agent ON nodes(agent_id);

CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  uuid TEXT NOT NULL,
  password TEXT NOT NULL,
  quota_bytes INTEGER NOT NULL DEFAULT 0,      -- 0 = 不限
  traffic_multiplier REAL NOT NULL DEFAULT 1.0,
  expire_at INTEGER,
  enabled INTEGER NOT NULL DEFAULT 1,
  -- true = 当前禁用由系统自动控制(超额)造成,可在月重置日自动解封;
  -- false = 管理员手动停用,任何自动流程都不会替他恢复。(沿用旧项目 006_auto_disabled 的语义)
  auto_disabled INTEGER NOT NULL DEFAULT 0,
  reset_day INTEGER,
  sub_token TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

-- 一个用户 ↔ 多个节点;节点可来自不同 agent(§0.0-3)
CREATE TABLE user_nodes (
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  PRIMARY KEY(user_id, node_id)
);

-- 每 (用户, 节点) 一行;跨 agent 求和得总量
CREATE TABLE user_traffic (
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  counter_epoch TEXT,
  last_up INTEGER NOT NULL DEFAULT 0,
  last_down INTEGER NOT NULL DEFAULT 0,
  cycle_up INTEGER NOT NULL DEFAULT 0,
  cycle_down INTEGER NOT NULL DEFAULT 0,
  total_up INTEGER NOT NULL DEFAULT 0,
  total_down INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY(user_id, node_id)
);
CREATE INDEX idx_user_traffic_user ON user_traffic(user_id);

CREATE TABLE agent_events (
  id INTEGER PRIMARY KEY,
  agent_id INTEGER,
  kind TEXT NOT NULL,
  message TEXT NOT NULL,
  at INTEGER NOT NULL
);
CREATE INDEX idx_agent_events_at ON agent_events(at);

-- 跨 agent 的用户总流量(§6.2)。
-- LEFT JOIN + COALESCE 是必须的:没有任何流量记录的用户也要出现(值为 0),
-- 否则用户列表会漏行。
CREATE VIEW user_traffic_total AS
SELECT u.id AS user_id,
       u.name AS name,
       COALESCE(SUM(t.cycle_up),   0) AS cycle_up,
       COALESCE(SUM(t.cycle_down), 0) AS cycle_down,
       COALESCE(SUM(t.total_up),   0) AS total_up,
       COALESCE(SUM(t.total_down), 0) AS total_down
FROM users u
LEFT JOIN user_traffic t ON t.user_id = u.id
GROUP BY u.id;
