-- 每台 agent 的网卡记账口径。原始 RX/TX 始终分别保存；这里只决定主控如何
-- 把本周期两个方向投影成配额用量和绑定订阅里的用量。
--
-- 默认 sum 保持升级前的 RX+TX 行为。模式切换只重算展示，不改原始计数、不清周期。
ALTER TABLE agents ADD COLUMN nic_accounting_mode TEXT NOT NULL DEFAULT 'sum';
