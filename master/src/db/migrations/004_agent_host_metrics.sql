-- agents 上报的主机指标一直被丢掉(DESIGN.md §7.2 / §8.0)。
--
-- `sysinfo.report` 里的 cpu_pct / mem_used / mem_total / load1 / uptime_secs 每 30 秒
-- 就到主控一次,但入库时只取了 nic 那一段,其余原地扔掉 —— 于是概览页想显示
-- 「哪台机器忙成什么样」时无米下锅,而数据其实早就在线上了。
--
-- 放在 agents 表而不是另起一张时序表:这几个值只有**最新一次**有用
-- (界面上是「现在的 CPU」),留历史等于给一张每 30 秒 × N 台 agent 增长的表,
-- 而没有任何一处会去查它。真要做趋势图,那是另一个设计,不是给这一列加历史。
--
-- 全部可空:从没连上过的 agent 这几列就该是 NULL,而不是一个看起来像真的 0
-- (与 nic_quota_bytes 用 NULL 表示「不限」同一个理由)。
ALTER TABLE agents ADD COLUMN cpu_pct REAL;
ALTER TABLE agents ADD COLUMN mem_used INTEGER;
ALTER TABLE agents ADD COLUMN mem_total INTEGER;
ALTER TABLE agents ADD COLUMN load1 REAL;
ALTER TABLE agents ADD COLUMN uptime_secs INTEGER;
-- 这几个值是哪一刻的。没有它,一台离线三天的机器会一直显示三天前那个 CPU 数字,
-- 看起来和在线的机器毫无区别。
ALTER TABLE agents ADD COLUMN sysinfo_at INTEGER;
