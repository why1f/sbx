-- 用户 ↔ agent 网卡流量绑定(DESIGN.md §10.3)。
--
-- 绑定之后,**只有订阅响应头 `subscription-userinfo` 里的流量**换成这些机器的
-- 网卡用量之和;订阅内容、用户自己的计费流量、界面上的用户用量都不受影响。
--
-- 这是给管理员自己用的:VPS 厂商按**网卡**计费,而用户流量是 sing-box 记的账,
-- 两者天生对不上(§6.4 / §7.2)。绑定之后管理员用任何一个代理客户端就能看到
-- 「这台机器这个月烧了多少」,不用 ssh 上去查。
--
-- 为什么是一张独立的表而不是 users 上加一列:一个用户可以绑多台机器
-- (管理员手里常常是「我名下这几台的总量」),和 user_nodes 是同一种多对多关系。
--
-- 为什么不推进任何 revision:这张表**不进 sing-box 配置**。绑定关系只被
-- 订阅那一条路读,agent 根本不需要知道它。
CREATE TABLE user_nic_bindings (
  user_id  INTEGER NOT NULL REFERENCES users(id)  ON DELETE CASCADE,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  PRIMARY KEY(user_id, agent_id)
);
CREATE INDEX idx_user_nic_bindings_user ON user_nic_bindings(user_id);
