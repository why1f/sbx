//! 在线 agent 的登记表(DESIGN.md §3 `cluster/registry.rs`)。
//!
//! 职责只有一件事:**`AgentId -> 当前那条连接的发送端**。
//! RPC 关联、心跳超时判定、落库都不在这里(分别是 `rpc.rs` / `server.rs` / `ingest.rs`)。
//!
//! 关键规则(§4.1):**同一 `agent_id` 的新连接到达时,立刻驱逐旧连接。**
//! `kill -9` 后重连、网络分区恢复都会走这条路径——旧连接的 TCP 可能几分钟后才超时,
//! 不主动驱逐就会出现「两个 conn 同时在表里,下发命令随机走一条」。

use sbx_shared::Envelope;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// 一条 agent 连接的发送端。写入即发往该 agent。
pub type ConnTx = mpsc::UnboundedSender<Envelope>;

/// 一条在线连接。
struct AgentConn {
    tx: ConnTx,
    /// 单调递增的连接序号。用来判断「我是不是已经被驱逐了」——
    /// 被驱逐的连接处理协程仍在运行(它要等自己的 socket 读到 EOF),
    /// 若它此时调用 `unregister`,不能把**新**连接踢掉。
    epoch: u64,
    /// 这条连接上**已经成功下发**的 `config_revision`。
    ///
    /// 巡检靠它判断「库里的配置比这台机器新吗」。存在连接上而不是库里,
    /// 是因为它描述的是**这条连接的状态**:断线重连之后一切从握手的
    /// catch_up 重新对齐,旧值跟着连接一起消失才是对的。
    ///
    /// `None` = 握手后还没在这条连接上下发过。此时以 agent 握手时报的
    /// revision 为准(由 `mark_config_sent` 在 catch_up 后写入)。
    sent_config_rev: Option<i64>,
    /// 同上,但对应 `user_state_revision`(禁用名单)。
    ///
    /// 两个 revision **各记各的**:改配置要重建 box,改禁用名单只翻内存标记,
    /// 合成一个会让「停用一个用户」退化成一次整机重建(§4.1)。
    sent_user_rev: Option<i64>,
}

#[derive(Default)]
pub struct Registry {
    conns: HashMap<i64, AgentConn>,
    next_epoch: u64,
}

/// `register` 的结果:告诉调用方是否发生了驱逐,以及自己的连接序号。
pub struct Registered {
    /// 自己的连接序号,`unregister` 时要带回来。
    pub epoch: u64,
    /// 是否驱逐了同一 agent 的一条旧连接。
    ///
    /// 为 true 时**不写 `agent_events`**——这是正常现象,不是故障(§4.1)。
    /// 但值得记一条 debug 日志,便于排查「agent 反复重连」这类问题。
    pub evicted_previous: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条新连接。同 id 的旧连接被**立刻**丢弃(§4.1)。
    ///
    /// 丢弃 `ConnTx` 会让对应的接收端拿到 `None`,那条连接的发送协程随即退出。
    pub fn register(&mut self, agent_id: i64, tx: ConnTx) -> Registered {
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        let evicted_previous = self
            .conns
            .insert(
                agent_id,
                AgentConn { tx, epoch, sent_config_rev: None, sent_user_rev: None },
            )
            .is_some();
        Registered { epoch, evicted_previous }
    }

    /// 记下「这台 agent 上已经生效的 config_revision」。
    ///
    /// 只在 `config.apply` **成功**之后调用 —— 失败时不记,让下一轮巡检重试。
    /// agent 不在表里(刚断开)时静默忽略:那条连接已经没了,记了也没人用。
    pub fn mark_config_sent(&mut self, agent_id: i64, rev: i64) {
        if let Some(c) = self.conns.get_mut(&agent_id) {
            c.sent_config_rev = Some(rev);
        }
    }

    /// 已下发的 config_revision。`None` = 这条连接上还没下发过。
    pub fn sent_config_rev(&self, agent_id: i64) -> Option<i64> {
        self.conns.get(&agent_id).and_then(|c| c.sent_config_rev)
    }

    /// 记下「这台 agent 上已经生效的 user_state_revision」。理由同 `mark_config_sent`。
    pub fn mark_user_sent(&mut self, agent_id: i64, rev: i64) {
        if let Some(c) = self.conns.get_mut(&agent_id) {
            c.sent_user_rev = Some(rev);
        }
    }

    /// 已下发的 user_state_revision。`None` = 这条连接上还没下发过。
    pub fn sent_user_rev(&self, agent_id: i64) -> Option<i64> {
        self.conns.get(&agent_id).and_then(|c| c.sent_user_rev)
    }

    /// 注销一条连接。
    ///
    /// **必须带上 `register` 返回的 `epoch`。** 若当前登记的是另一条更新的连接,
    /// 这次注销会被忽略并返回 false——否则旧连接的清理会把刚接上的新连接踢下线,
    /// 表现为「agent 连上后立刻掉线,再连再掉」的循环。
    pub fn unregister(&mut self, agent_id: i64, epoch: u64) -> bool {
        match self.conns.get(&agent_id) {
            Some(c) if c.epoch == epoch => {
                self.conns.remove(&agent_id);
                true
            }
            _ => false,
        }
    }

    /// 取某 agent 当前连接的发送端。离线时为 `None`。
    pub fn get(&self, agent_id: i64) -> Option<&ConnTx> {
        self.conns.get(&agent_id).map(|c| &c.tx)
    }

    /// 某 agent 当前是否有活连接。**这是内存态,不是 `agents.status` 字段** ——
    /// 后者可能因为进程崩溃残留 online(daemon 启动时会清一次)。
    /// TUI(§8)的在线标记读这个。
    #[allow(dead_code)]
    pub fn is_online(&self, agent_id: i64) -> bool {
        self.conns.contains_key(&agent_id)
    }

    /// 向某 agent 发一条消息。agent 离线或通道已关闭时返回 false。
    ///
    /// **发送失败不是错误路径**:§6.3 明确规定离线 agent 的变更留到重连握手时补齐,
    /// 所以调用方通常只需要「没发出去」这一个事实,不需要区分原因。
    pub fn send(&self, agent_id: i64, env: Envelope) -> bool {
        self.get(agent_id).is_some_and(|tx| tx.send(env).is_ok())
    }

    /// 当前在线的 agent id。
    pub fn online_ids(&self) -> Vec<i64> {
        self.conns.keys().copied().collect()
    }

    /// 在线连接数。TUI(§8)的 dashboard 读它;测试里也用来断言驱逐后不留残连接。
    #[allow(dead_code)]
    pub fn online_count(&self) -> usize {
        self.conns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbx_shared::method;

    fn env() -> Envelope {
        Envelope::event(method::PONG, serde_json::json!({"echo_ts": 1}))
    }

    fn chan() -> (ConnTx, mpsc::UnboundedReceiver<Envelope>) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn register_then_send_reaches_that_connection() {
        let mut r = Registry::new();
        let (tx, mut rx) = chan();
        let reg = r.register(1, tx);

        assert!(!reg.evicted_previous, "首次登记不该报驱逐");
        assert!(r.is_online(1));
        assert!(r.send(1, env()));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn sending_to_offline_agent_is_false_not_panic() {
        let r = Registry::new();
        assert!(!r.is_online(99));
        assert!(!r.send(99, env()), "离线时应返回 false");
        assert!(r.get(99).is_none());
    }

    /// §4.1:同 id 的新连接必须驱逐旧连接,否则会出现「两个 conn 同时在表里」。
    #[test]
    fn new_connection_evicts_the_previous_one() {
        let mut r = Registry::new();
        let (tx1, mut rx1) = chan();
        let (tx2, mut rx2) = chan();

        r.register(1, tx1);
        let second = r.register(1, tx2);
        assert!(second.evicted_previous, "同 id 重连应报告驱逐");
        assert_eq!(r.online_count(), 1, "不能出现两条连接同时在表里");

        // 发送只应到达新连接
        assert!(r.send(1, env()));
        assert!(rx2.try_recv().is_ok(), "新连接应收到");
        assert!(rx1.try_recv().is_err(), "旧连接不该再收到任何东西");
    }

    /// 被驱逐的旧连接随后清理自己时,**不能**把新连接踢下线。
    /// 这是「agent 连上后立刻掉线,再连再掉」那个循环的成因。
    #[test]
    fn stale_unregister_does_not_evict_the_live_connection() {
        let mut r = Registry::new();
        let (tx1, _rx1) = chan();
        let (tx2, mut rx2) = chan();

        let first = r.register(1, tx1);
        let second = r.register(1, tx2);

        // 旧连接的处理协程这时才发现 socket 断了,来注销自己
        assert!(!r.unregister(1, first.epoch), "陈旧的注销应被忽略");
        assert!(r.is_online(1), "新连接必须还在线");
        assert!(r.send(1, env()));
        assert!(rx2.try_recv().is_ok());

        // 新连接自己注销才真的下线
        assert!(r.unregister(1, second.epoch));
        assert!(!r.is_online(1));
    }

    #[test]
    fn send_fails_after_receiver_dropped() {
        let mut r = Registry::new();
        let (tx, rx) = chan();
        r.register(1, tx);
        drop(rx);
        assert!(!r.send(1, env()), "通道已关闭时应返回 false 而不是 panic");
    }

    #[test]
    fn tracks_multiple_agents_independently() {
        let mut r = Registry::new();
        let (tx1, mut rx1) = chan();
        let (tx2, mut rx2) = chan();
        r.register(1, tx1);
        r.register(2, tx2);

        assert_eq!(r.online_count(), 2);
        let mut ids = r.online_ids();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);

        assert!(r.send(1, env()));
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err(), "不该串到另一个 agent");
    }
}
