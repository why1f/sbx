//! epoch / delta 增量算法(DESIGN.md §5.2)。
//!
//! 这是全系统**最容易悄悄算错**的一块:错了不会崩,只会让流量数字慢慢偏,
//! 而且偏得像是「用户真的跑了那么多」。所以它被单独拆成一个纯函数 + 一组表驱动测试。
//!
//! ```text
//! epoch 相同 且 new >= last   →   delta = new - last
//! 其他任何情况                →   delta = new        (视作新纪元的首次上报)
//! ```
//!
//! 两个 epoch 的来源(§5.2):
//!   * 用户流量 → `counter_epoch`,agent **进程**启动时生成的 UUID
//!   * 网卡流量 → `boot_id`,取自 `/proc/sys/kernel/random/boot_id`

/// 一次上报相对上次的增量结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delta {
    /// 本次要累进到 cycle_* / total_* 的增量。**保证 >= 0**。
    pub delta: i64,
    /// 是否发生了 epoch 变更(或计数器回绕)。
    ///
    /// 为 true 时主控必须写一条 `agent_events` 审计记录、TUI 显示「计数器已重置」,
    /// **不能静默吞掉**(§5.4)——静默的状态跃迁会让「流量数字不对」无法归因。
    pub epoch_changed: bool,
}

/// 计算单个计数器的增量。
///
/// `last_epoch` 为 `None` 表示「从未收到过上报」(库里是 NULL),按新纪元处理。
///
/// 注意这个函数**不判断配额、不做月重置**——那是 `service/` 的事。
/// 它只回答一个问题:这次上报比上次多了多少。
pub fn compute(last_epoch: Option<&str>, last: i64, new_epoch: &str, new: i64) -> Delta {
    let same_epoch = last_epoch == Some(new_epoch);
    if same_epoch && new >= last {
        Delta { delta: new - last, epoch_changed: false }
    } else {
        // 三种情况落在这里:
        //   1. epoch 变了(agent 进程重启 / 机器重启)
        //   2. 首次上报(last_epoch 是 None)
        //   3. epoch 没变但 new < last —— 这**不该发生**(计数器在进程内单调递增)。
        //      发生了说明有 bug 或数据损坏,当作新纪元是保守的选择:
        //      宁可少记一段(delta = new),也不要记出一个负数把总量拉低。
        Delta {
            delta: new.max(0),
            // 首次上报(last_epoch 为 None 且 last 为 0)不算「重置」,
            // 否则每个新 agent 上线都会刷一条无意义的审计记录。
            epoch_changed: !(last_epoch.is_none() && last == 0),
        }
    }
}

/// 上下行成对计算的便利包装。up/down 共享同一个 epoch。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairDelta {
    pub up: i64,
    pub down: i64,
    pub epoch_changed: bool,
}

pub fn compute_pair(
    last_epoch: Option<&str>,
    last_up: i64,
    last_down: i64,
    new_epoch: &str,
    new_up: i64,
    new_down: i64,
) -> PairDelta {
    let u = compute(last_epoch, last_up, new_epoch, new_up);
    let d = compute(last_epoch, last_down, new_epoch, new_down);
    PairDelta {
        up: u.delta,
        down: d.delta,
        // 两路共享 epoch,任一路判定为变更即视为变更(实际上必然同时变更)。
        epoch_changed: u.epoch_changed || d.epoch_changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §13.1 要求的三类输入:正常递增、epoch 变更、`new < last` 回绕。
    #[test]
    fn table_driven_three_required_cases() {
        struct Case {
            what: &'static str,
            last_epoch: Option<&'static str>,
            last: i64,
            new_epoch: &'static str,
            new: i64,
            want: i64,
            want_changed: bool,
        }
        let cases = [
            Case {
                what: "正常递增",
                last_epoch: Some("e1"),
                last: 100,
                new_epoch: "e1",
                new: 250,
                want: 150,
                want_changed: false,
            },
            Case {
                what: "epoch 变更(agent 重启),新纪元首次上报全额计入",
                last_epoch: Some("e1"),
                last: 100,
                new_epoch: "e2",
                new: 30,
                want: 30,
                want_changed: true,
            },
            Case {
                what: "epoch 未变但 new < last(不该发生),按新纪元保守处理",
                last_epoch: Some("e1"),
                last: 500,
                new_epoch: "e1",
                new: 20,
                want: 20,
                want_changed: true,
            },
            Case {
                what: "首次上报:不算重置,不该刷审计记录",
                last_epoch: None,
                last: 0,
                new_epoch: "e1",
                new: 77,
                want: 77,
                want_changed: false,
            },
            Case {
                what: "epoch 相同且完全没动",
                last_epoch: Some("e1"),
                last: 42,
                new_epoch: "e1",
                new: 42,
                want: 0,
                want_changed: false,
            },
        ];
        for c in cases {
            let got = compute(c.last_epoch, c.last, c.new_epoch, c.new);
            assert_eq!(got.delta, c.want, "{}: delta", c.what);
            assert_eq!(got.epoch_changed, c.want_changed, "{}: epoch_changed", c.what);
        }
    }

    /// 增量永不为负 —— 这是整个算法存在的理由。
    /// 天真的 `new - last` 在 epoch 变更时会给出负数,把用户总量往回拉。
    #[test]
    fn delta_is_never_negative() {
        for (le, last, ne, new) in [
            (Some("e1"), 1_000_000i64, "e2", 0i64),
            (Some("e1"), i64::MAX, "e1", 0),
            (None, 0, "e1", 0),
            (Some("e1"), 5, "e1", 5),
        ] {
            assert!(compute(le, last, ne, new).delta >= 0, "delta 不能为负: {le:?} {last} {ne} {new}");
        }
    }

    /// 机器重启场景:boot_id 变化 + 网卡计数器清零。
    /// 用同一个函数处理网卡流量(§6.4「delta 算法与用户流量同一个」)。
    #[test]
    fn nic_counters_after_reboot_use_new_value() {
        let d = compute_pair(Some("boot-old"), 900_000, 800_000, "boot-new", 1_024, 2_048);
        assert_eq!(d.up, 1_024);
        assert_eq!(d.down, 2_048);
        assert!(d.epoch_changed, "boot_id 变化必须可见(§5.4)");
    }

    /// 连续多轮上报的累计结果应等于「最后一次的值」——
    /// 即增量求和不会因为分多次上报而漂移。
    #[test]
    fn successive_deltas_accumulate_to_final_value() {
        let mut acc = 0i64;
        let mut last = 0i64;
        for new in [10, 25, 25, 100, 3_000] {
            acc += compute(Some("e1").filter(|_| last > 0), last, "e1", new).delta;
            last = new;
        }
        assert_eq!(acc, 3_000, "增量之和应等于最终累计值");
    }

    /// 跨一次进程重启的累计:重启前已上报的部分**不能丢失**,
    /// 重启后的部分**不能重复计**(§13.3)。
    #[test]
    fn accumulation_survives_a_restart_without_loss_or_double_count() {
        // 纪元 1:涨到 500,分两次上报
        let mut acc = 0i64;
        acc += compute(None, 0, "e1", 200).delta; // 首次
        acc += compute(Some("e1"), 200, "e1", 500).delta;
        assert_eq!(acc, 500);

        // agent 被 kill -9 后重启 → 新 epoch,计数从 0 重新开始,首报 70
        let d = compute(Some("e1"), 500, "e2", 70);
        assert!(d.epoch_changed);
        acc += d.delta;
        assert_eq!(acc, 570, "重启前的 500 保住,重启后的 70 计一次");

        // 新纪元内继续涨
        acc += compute(Some("e2"), 70, "e2", 130).delta;
        assert_eq!(acc, 630);
    }
}
