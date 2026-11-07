-- 删除节点保留用户账目及累计计数基线。agent id 不复用,用户删除时才清理归档。
CREATE TABLE IF NOT EXISTS retired_user_traffic (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    agent_id INTEGER NOT NULL,
    node_tag TEXT NOT NULL,
    agent_name TEXT NOT NULL,
    counter_epoch TEXT,
    last_up INTEGER NOT NULL,
    last_down INTEGER NOT NULL,
    cycle_up INTEGER NOT NULL,
    cycle_down INTEGER NOT NULL,
    total_up INTEGER NOT NULL,
    total_down INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, agent_id, node_tag)
);

DROP VIEW user_traffic_total;
-- 归档按 user_id 索引查询,避免单用户订阅查询先物化全站历史记录。
CREATE VIEW user_traffic_total AS
SELECT u.id AS user_id, u.name AS name,
       COALESCE(SUM(t.cycle_up), 0) + COALESCE(
           (SELECT SUM(r.cycle_up) FROM retired_user_traffic r WHERE r.user_id = u.id), 0) AS cycle_up,
       COALESCE(SUM(t.cycle_down), 0) + COALESCE(
           (SELECT SUM(r.cycle_down) FROM retired_user_traffic r WHERE r.user_id = u.id), 0) AS cycle_down,
       COALESCE(SUM(t.total_up), 0) + COALESCE(
           (SELECT SUM(r.total_up) FROM retired_user_traffic r WHERE r.user_id = u.id), 0) AS total_up,
       COALESCE(SUM(t.total_down), 0) + COALESCE(
           (SELECT SUM(r.total_down) FROM retired_user_traffic r WHERE r.user_id = u.id), 0) AS total_down
FROM users u
LEFT JOIN user_traffic t ON t.user_id = u.id
GROUP BY u.id;
