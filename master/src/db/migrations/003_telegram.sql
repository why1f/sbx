-- Telegram Bot 相关状态(DESIGN.md §9.1,对应旧项目 005_telegram + 007_tg_bot_lease)。
--
-- 三块东西:
--   1. users 上的 tg_* 列:绑定关系、每个用户自己的通知开关与时间表、去重游标;
--   2. tg_admin_prefs:管理员侧的同类偏好(管理员不是 users 表里的行);
--   3. tg_bot_lease:单实例租约。

-- 绑定关系。0 = 未绑定 —— 用 0 而不是 NULL,因为 chat_id 是 i64,
-- 判定「有没有绑」用一次比较就够,不必到处 Option。
ALTER TABLE users ADD COLUMN tg_chat_id INTEGER NOT NULL DEFAULT 0;
-- 绑定码。用户在 bot 里发 /bind <码> 完成绑定;绑定后清空,防止重放。
ALTER TABLE users ADD COLUMN tg_bind_token TEXT NOT NULL DEFAULT '';

-- 三档阈值各自可关。默认全开:通知的价值在于「快用完了」这件事本身,
-- 想静音的人自己关,而不是让所有人先去打开。
ALTER TABLE users ADD COLUMN tg_notify_quota_80 INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN tg_notify_quota_90 INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN tg_notify_quota_100 INTEGER NOT NULL DEFAULT 1;

-- 定时播报。times 为空数组时用配置里的默认时间表。
ALTER TABLE users ADD COLUMN tg_schedule_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN tg_schedule_times TEXT NOT NULL DEFAULT '[]';

-- 已经通知到哪一档(0/80/90/100)。**这是告警去重的全部机制**:
-- 只在档位**上升**时推送,所以 80% 那条不会每 30 秒重发一次。
-- 月重置或管理员调高配额让百分比掉回去时,由 normalize_quota_levels 把它降下来,
-- 下次再涨上去才会重新通知。
ALTER TABLE users ADD COLUMN tg_last_quota_level INTEGER NOT NULL DEFAULT 0;
-- {"09:00":"2026-08-01"}:每个时间点最后一次播报的日期。
-- 记日期而不是时间戳,是因为判定条件是「今天这个点推过没有」——
-- 巡检每 30s 跑一次,同一分钟内会命中两次。
ALTER TABLE users ADD COLUMN tg_last_schedule_dates TEXT NOT NULL DEFAULT '{}';

CREATE TABLE IF NOT EXISTS tg_admin_prefs (
    chat_id             INTEGER PRIMARY KEY,
    notify_quota        INTEGER NOT NULL DEFAULT 1,
    schedule_enabled    INTEGER NOT NULL DEFAULT 1,
    schedule_times      TEXT    NOT NULL DEFAULT '[]',
    last_schedule_dates TEXT    NOT NULL DEFAULT '{}'
);

-- Telegram Bot 单实例租约。
--
-- 同一个 bot_token 只允许一个 getUpdates 长轮询:多开时 Telegram 会对其中一个
-- 返回 409 Conflict,两边都会随机丢 update,表现为「命令时灵时不灵」。
-- daemon 与 TUI 都可能起 bot(服务在跑、管理员又开了界面),
-- 用这张表做跨进程互斥:拿到租约的实例才跑 bot,其余跳过。
--
-- 租约靠心跳续期;持有者被 kill 后心跳停止,超时即可被接管。
CREATE TABLE IF NOT EXISTS tg_bot_lease (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    owner     TEXT    NOT NULL,
    heartbeat INTEGER NOT NULL
);
