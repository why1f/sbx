-- agents.id 改用 AUTOINCREMENT,杜绝**删除后 id 被复用**(v0.4.9)。
--
-- ## 为什么这是个 bug 而不是洁癖
--
-- SQLite 的 `INTEGER PRIMARY KEY` 不带 AUTOINCREMENT 时,新行取的是
-- 「当前最大 id + 1」—— 删掉最大那个,下一次插入就会**把它捡回来**。
--
-- 而 TUI 与 daemon 是两个进程,只共享这个库(§8.0)。在 TUI 里删掉一台 agent,
-- daemon 内存里那条 WebSocket 毫不知情、继续跑;等你重新添加一台、它复用了
-- 同一个 id,那条老连接的下一次 `sysinfo.report` 就写进了**新 agent 的行**。
--
-- 现场表现:新加的机器还没跑安装命令、页面上显示离线,上下行却在涨;
-- `config_revision` 也被那条老连接推着往上走。数字全是上一台机器的。
--
-- AUTOINCREMENT 让 id 严格单调:删掉的 id 永不再发,老连接带着的 agent_id
-- 在库里查不到,daemon 侧随即把它断开(见 server.rs 的存在性检查)。
--
-- ## 为什么要重建表
--
-- AUTOINCREMENT 是建表时的属性,ALTER TABLE 改不了,只能新建 + 搬数据 + 改名。
-- 搬完之后 `sqlite_sequence` 里那条水位要**手工推到当前最大 id**,
-- 否则下一个新 agent 又会从 1 开始发,等于什么都没改。
--
-- ## 外键为什么不在这个文件里关
--
-- `PRAGMA foreign_keys` 在**事务内是空操作**,而这里每一版都跑在一个事务里
-- (见 db/mod.rs 的迁移执行器)。写在这儿只会让人以为关掉了,实际没有。
--
-- 而它必须真的关掉:外键开着时 `DROP TABLE agents` 会先做一次**隐式 DELETE**,
-- 那会顺着 ON DELETE CASCADE 把 nodes / agent_nic_traffic / user_nodes /
-- user_traffic 全部清空 —— 一次「改个自增属性」的迁移把所有节点和流量删干净。
--
-- 所以迁移连接在建立时就带 `foreign_keys(false)`(db/mod.rs 的 `migrate`),
-- 整段仍在一个事务里,中途出错会整体回滚,不会留下半张表。

CREATE TABLE agents_new (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  token_hash TEXT NOT NULL,
  token_prefix TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'never',
  last_seen INTEGER,
  agent_version TEXT,
  singbox_version TEXT,
  os TEXT,
  arch TEXT,
  hostname TEXT,
  ipv4 TEXT,
  ipv6 TEXT,
  nic_quota_bytes INTEGER,
  nic_reset_day INTEGER,
  config_revision INTEGER NOT NULL DEFAULT 0,
  user_state_revision INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  outbound_strategy TEXT NOT NULL DEFAULT 'auto',
  -- 004 加的六个主机指标列。**漏掉任何一列,重建后的表就永久缺它** ——
  -- 列清单必须是 001 + 004 + 008 的并集。
  cpu_pct REAL,
  mem_used INTEGER,
  mem_total INTEGER,
  load1 REAL,
  uptime_secs INTEGER,
  sysinfo_at INTEGER
);

INSERT INTO agents_new
  (id, name, token_hash, token_prefix, status, last_seen, agent_version,
   singbox_version, os, arch, hostname, ipv4, ipv6, nic_quota_bytes,
   nic_reset_day, config_revision, user_state_revision, created_at, outbound_strategy,
   cpu_pct, mem_used, mem_total, load1, uptime_secs, sysinfo_at)
SELECT
   id, name, token_hash, token_prefix, status, last_seen, agent_version,
   singbox_version, os, arch, hostname, ipv4, ipv6, nic_quota_bytes,
   nic_reset_day, config_revision, user_state_revision, created_at,
   COALESCE(outbound_strategy, 'auto'),
   cpu_pct, mem_used, mem_total, load1, uptime_secs, sysinfo_at
FROM agents;

DROP TABLE agents;
ALTER TABLE agents_new RENAME TO agents;
CREATE INDEX idx_agents_token_prefix ON agents(token_prefix);

-- 水位推到当前最大 id。少了这一步,空库之外的场景下一个新 agent 会拿到
-- 一个已经用过的 id —— 正是这次要修的东西。
-- (AUTOINCREMENT 表在插入后才会自动建 sqlite_sequence 行,所以用 INSERT OR REPLACE。)
INSERT OR REPLACE INTO sqlite_sequence (name, seq)
SELECT 'agents', COALESCE((SELECT MAX(id) FROM agents), 0);
