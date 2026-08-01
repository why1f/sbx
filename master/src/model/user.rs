//! 用户模型。移植自旧项目 `src/model/user.rs`(DESIGN.md §9.1)。
//!
//! 保留的语义:`used_total_bytes` / `quota_bytes` / `quota_used_percent` /
//! `is_expired` / `is_over_quota` / `format_bytes`。
//!
//! 相对旧项目的改动:
//!   * `quota_gb: f64` → `quota_bytes: i64`(库里直接存字节,不再每次乘 1073741824)
//!   * `used_*_bytes` 不再是 users 表的列 —— 流量按 (用户, 节点) 存在 `user_traffic`,
//!     跨 agent 求和从 `user_traffic_total` 视图读(§6.2)。于是配额相关方法改为**接受用量参数**。
//!   * `allow_all_nodes` / `allowed_nodes` JSON 列 → `user_nodes` 关联表(§6.1),
//!     `can_use_node(tag)` 因此从模型方法变成一次查询,不在这里。
//!   * `expire_at: String("YYYY-MM-DD")` → `Option<i64>` unix 秒。
//!     旧项目用字符串比日期,每次都要 parse 且失败时静默当作「未过期」;
//!     存时间戳后 `is_expired` 变成一次整数比较,不会失败。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub uuid: String,
    pub password: String,
    /// 0 = 不限。
    pub quota_bytes: i64,
    pub traffic_multiplier: f64,
    /// unix 秒。None = 永不过期。
    pub expire_at: Option<i64>,
    pub enabled: bool,
    /// true = 当前禁用由系统自动控制(超额)造成,可在月重置日自动解封;
    /// false = 管理员手动停用,任何自动流程都不会替他恢复。
    /// (沿用旧项目 `auto_disabled` 的语义,它避免了「管理员手动封的人被月重置放出来」)
    pub auto_disabled: bool,
    pub reset_day: Option<i64>,
    pub sub_token: String,
    pub created_at: i64,
}

/// 一个用户的用量(已跨 agent 求和,来自 `user_traffic_total` 视图)。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, sqlx::FromRow)]
pub struct Usage {
    pub cycle_up: i64,
    pub cycle_down: i64,
    pub total_up: i64,
    pub total_down: i64,
}

impl Usage {
    pub fn cycle_raw(&self) -> i64 {
        self.cycle_up + self.cycle_down
    }
}

impl User {
    /// 计费用量 = (上行 + 下行) × 倍率。配额判定用**本周期**用量(§6.3)。
    pub fn used_total_bytes(&self, usage: &Usage) -> i64 {
        (usage.cycle_raw() as f64 * self.traffic_multiplier) as i64
    }

    pub fn quota_used_percent(&self, usage: &Usage) -> f64 {
        if self.quota_bytes == 0 {
            return 0.0;
        }
        (self.used_total_bytes(usage) as f64 / self.quota_bytes as f64 * 100.0).min(100.0)
    }

    /// 判定规则本体见 [`over_quota`] —— supervisor 的巡检(§6.3)调的是那个自由函数,
    /// 因为它拿到的是 SQL join 出来的裸数字而不是 `User`。两边共用一份定义,
    /// 否则「配额到底算不算等于」这种细节会在两处慢慢漂开。
    #[allow(dead_code)] // TUI(§8)会用;规则本身由 over_quota 持有,不是死代码
    pub fn is_over_quota(&self, usage: &Usage) -> bool {
        over_quota(self.quota_bytes, self.used_total_bytes(usage))
    }

    /// `now` 由调用方传入(unix 秒),不在这里读时钟——
    /// 否则这个方法无法被测试,而「到期判定」正是最需要测的东西之一。
    #[allow(dead_code)] // 同上,规则本体是 expired()
    pub fn is_expired(&self, now: i64) -> bool {
        expired(self.expire_at, now)
    }

    /// 是否应当被自动禁用(§6.3)。**只看自动条件**,不看 `enabled` 现值。
    #[allow(dead_code)] // TUI(§8)的用户页会用它标注「即将被自动停用」
    pub fn should_auto_disable(&self, usage: &Usage, now: i64) -> bool {
        self.is_over_quota(usage) || self.is_expired(now)
    }

    pub fn format_bytes(bytes: i64) -> String {
        const TB: i64 = 1_099_511_627_776;
        const GB: i64 = 1_073_741_824;
        const MB: i64 = 1_048_576;
        const KB: i64 = 1_024;
        match bytes {
            b if b >= TB => format!("{:.2} TB", b as f64 / TB as f64),
            b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
            b if b >= MB => format!("{:.2} MB", b as f64 / MB as f64),
            b if b >= KB => format!("{:.2} KB", b as f64 / KB as f64),
            b => format!("{} B", b),
        }
    }
}

// ─────────────── 自动停用的判定规则(单一定义)───────────────
//
// 这两个函数是**规则本体**。有两个调用面:
//   * `User::is_over_quota` / `User::is_expired` —— 拿到完整 User 的地方(TUI);
//   * `supervisor::evaluate_quotas` —— 巡检时拿到的是 SQL join 出来的裸数字,
//     构造不出 User(用量来自 user_traffic_total 视图,不是 User 的字段)。
//
// 写成自由函数就是为了让这两个面共用一份定义。规则里的边界条件
// (配额是 `>=` 不是 `>`、`quota_bytes == 0` 表示不限、到期是 `now > exp` 不是 `>=`)
// 一旦在两处各写一遍,就会在某次改动里悄悄漂开,而这类漂移的表现是计费不一致 —— 不报错。

/// 配额是否已用尽。`quota_bytes == 0` 表示**不限流量**,永远返回 false。
///
/// 用 `>=`:用满配额那一刻就该停,不必多放一个字节。
pub fn over_quota(quota_bytes: i64, used: i64) -> bool {
    quota_bytes > 0 && used >= quota_bytes
}

/// 是否已过期。`None` = 永不过期。
///
/// 用 `now > exp` 而不是 `>=`:`expire_at` 当作「有效到这一刻为止」。
pub fn expired(expire_at: Option<i64>, now: i64) -> bool {
    expire_at.is_some_and(|exp| now > exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: i64 = 1_073_741_824;

    fn user(quota_bytes: i64, mult: f64, expire_at: Option<i64>) -> User {
        User {
            id: 1,
            name: "alice".into(),
            uuid: "u".into(),
            password: "p".into(),
            quota_bytes,
            traffic_multiplier: mult,
            expire_at,
            enabled: true,
            auto_disabled: false,
            reset_day: None,
            sub_token: "t".into(),
            created_at: 0,
        }
    }

    fn usage(up: i64, down: i64) -> Usage {
        Usage { cycle_up: up, cycle_down: down, total_up: up, total_down: down }
    }

    #[test]
    fn quota_zero_means_unlimited() {
        let u = user(0, 1.0, None);
        let big = usage(999 * GB, 999 * GB);
        assert_eq!(u.quota_used_percent(&big), 0.0);
        assert!(!u.is_over_quota(&big));
    }

    #[test]
    fn multiplier_scales_billed_usage() {
        let u = user(10 * GB, 2.0, None);
        let usg = usage(3 * GB, 2 * GB); // 原始 5 GB
        assert_eq!(u.used_total_bytes(&usg), 10 * GB, "5 GB × 2.0");
        assert!(u.is_over_quota(&usg), "倍率后刚好打满应算超额");
    }

    #[test]
    fn percent_clamps_at_100() {
        let u = user(GB, 1.0, None);
        assert_eq!(u.quota_used_percent(&usage(10 * GB, 0)), 100.0);
    }

    #[test]
    fn expiry_is_a_plain_comparison_and_none_never_expires() {
        assert!(!user(0, 1.0, None).is_expired(i64::MAX), "None = 永不过期");
        let u = user(0, 1.0, Some(1_000));
        assert!(!u.is_expired(999));
        assert!(!u.is_expired(1_000), "到期当刻不算过期");
        assert!(u.is_expired(1_001));
    }

    /// 自动禁用只看自动条件(超额 / 到期),两者任一命中即可(§6.3)。
    #[test]
    fn should_auto_disable_on_either_condition() {
        let over = user(GB, 1.0, None);
        assert!(over.should_auto_disable(&usage(2 * GB, 0), 0));

        let expired = user(0, 1.0, Some(100));
        assert!(expired.should_auto_disable(&usage(0, 0), 200));

        let fine = user(10 * GB, 1.0, Some(1_000));
        assert!(!fine.should_auto_disable(&usage(GB, 0), 500));
    }

    /// 用量取自跨 agent 求和的视图,所以「多节点之和」必须能正确触发配额。
    /// 这是 §0.0(3) 与 §6.3 的交汇点。
    #[test]
    fn quota_triggers_on_cross_agent_sum() {
        let u = user(10 * GB, 1.0, None);
        // 单看任一节点都没超,但视图求和后超了
        let summed = usage(4 * GB, 7 * GB);
        assert!(u.is_over_quota(&summed), "配额判定应基于跨 agent 求和");
    }

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(User::format_bytes(0), "0 B");
        assert_eq!(User::format_bytes(1_023), "1023 B");
        assert_eq!(User::format_bytes(1_024), "1.00 KB");
        assert_eq!(User::format_bytes(1_048_576), "1.00 MB");
        assert_eq!(User::format_bytes(GB), "1.00 GB");
        assert_eq!(User::format_bytes(36_814_882_406), "34.29 GB", "对齐 1.png 的显示");
        assert_eq!(User::format_bytes(1_099_511_627_776), "1.00 TB");
    }
}
