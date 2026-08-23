-- 网卡月配额的重置边界改成按每台 agent 的时区算。
--
-- 为什么需要:每台被控的 VPS 厂商按**各自机房本地时间**的日界计费,而在这之前
-- 全部 agent 共用主控一个时区。现场实测:一台 UTC-7 的机器,厂商与第三方探针都在
-- 22 号 00:00 本地时间翻月,主控跑在 UTC 却在 00:00 UTC 翻月 —— 多统计 7 小时,
-- 读数高出约 13%,而两边读的其实是同一块网卡(基线只差百万分之 5.7)。
--
-- 为什么存偏移秒数而不是时区名:主控不引 chrono-tz(不把整个 tzdata 编进二进制,
-- 见 tg/fmt.rs 的同一条约束),所以只认 UTC 偏移。夏令时靠 agent 每次握手重报来跟随。
--
-- 两列的优先级:nic_reset_offset_secs(人手工填) > reported_utc_offset_secs(agent 上报)
--   > 主控本机时区。三层各有理由:
--   * 手工覆盖 —— agent 本机时区常常**不等于**厂商计费时区(VPS 镜像出厂多为 UTC);
--   * agent 上报 —— 新机器接入即对齐,不必记得去填;
--   * 主控本机 —— 老版本 agent 什么都不报,两列都是 NULL,行为与升级前逐字节一致。
ALTER TABLE agents ADD COLUMN reported_utc_offset_secs INTEGER;
ALTER TABLE agents ADD COLUMN nic_reset_offset_secs INTEGER;
