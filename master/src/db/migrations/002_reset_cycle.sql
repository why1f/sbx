-- 月重置需要记住「上次重置发生在哪个月」,否则同一天内每 30s 的巡检会反复重置。
-- 沿用旧项目 `last_reset_ym` 的语义:存 "YYYY-MM",空串 = 从未重置过。
--
-- 为什么不用 cycle_start 判断:cycle_start 在 user_traffic 里是**每节点一行**的,
-- 而重置是**按用户**的。用它判断会在「用户有多个节点且它们的 cycle_start 不同」时
-- 得到一个无法收敛的状态。

ALTER TABLE users ADD COLUMN last_reset_ym TEXT NOT NULL DEFAULT '';

ALTER TABLE agent_nic_traffic ADD COLUMN last_reset_ym TEXT NOT NULL DEFAULT '';
