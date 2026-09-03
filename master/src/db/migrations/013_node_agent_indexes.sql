-- 按 node_id / agent_id 查的热路径与级联删除,以前只能全表扫。
--
-- `user_nodes` 与 `user_traffic` 的主键是 (user_id, node_id),`user_traffic` 另有
-- 一个 user_id 索引;按 node_id 查的没有:
--   * 节点页每秒一次、每个节点两条相关子查询(tui/data.rs 里 `WHERE un.node_id = n.id`
--     与 `WHERE t.node_id = n.id`);
--   * `DELETE FROM nodes` 的 ON DELETE CASCADE 要在两张表里找 node_id。
-- 几十行时无所谓,几百行时是 O(节点数 × 行数),而且是在 TUI 的渲染路径上。
-- `agent_commands` / `user_nic_bindings` 的 agent_id 同理,只是热度低得多。
CREATE INDEX IF NOT EXISTS idx_user_nodes_node       ON user_nodes(node_id);
CREATE INDEX IF NOT EXISTS idx_user_traffic_node     ON user_traffic(node_id);
CREATE INDEX IF NOT EXISTS idx_agent_commands_agent  ON agent_commands(agent_id);
CREATE INDEX IF NOT EXISTS idx_user_nic_bindings_agt ON user_nic_bindings(agent_id);
