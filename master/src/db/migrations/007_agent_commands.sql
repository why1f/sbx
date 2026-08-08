-- TUI 给 daemon 递一次性指令的队列(目前只有 agent 升级)。
--
-- ## 为什么需要这张表
--
-- `sbx tui` 与 `sbx daemon` 是**两个独立进程**,只通过这个数据库交换状态。
-- WS 连接活在 daemon 里,TUI 手上没有 registry 也没有 rpc —— 它**没法**
-- 直接给 agent 发 `agent.upgrade`。
--
-- 配置类的变更靠 revision 递增来传达(agent 下次握手时比对),但升级不是状态、
-- 是一次性动作:它不该在 agent 每次重连时被重放一遍。所以走一张显式的队列表,
-- 由 daemon 的巡检循环取走执行,执行完就地记结果。
--
-- ## 为什么带结果而不是做完就删
--
-- 升级失败的信息是唯一能让人知道「为什么那台还是旧版本」的东西。
-- 做完就删的话,界面上只会看到版本号没变,而查不出是没下发、下发失败,
-- 还是 agent 那边校验没过。
CREATE TABLE agent_commands (
  id INTEGER PRIMARY KEY,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  -- 目前只有 'upgrade'。留成文本是为了以后加别的一次性指令时不用再开一张表。
  kind TEXT NOT NULL,
  -- 指令参数(升级是 {"url":…, "sha256":…})。由**入队方**拼好:
  -- 版本、产物地址、校验和都是主控侧的知识,daemon 只负责转发。
  payload_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  -- NULL = 还没执行。取走时置为当前时间,避免两轮巡检重复下发。
  taken_at INTEGER,
  done_at INTEGER,
  -- 空 = 成功;否则是错误原因,原样给界面看。
  error TEXT
);

-- 巡检每轮都要问一次「有没有待办」,这条索引让它是 O(1) 而不是全表扫。
CREATE INDEX idx_agent_commands_pending ON agent_commands(taken_at) WHERE taken_at IS NULL;
