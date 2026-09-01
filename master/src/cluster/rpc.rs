//! 请求/响应关联(DESIGN.md §3 `cluster/rpc.rs`)。
//!
//! 主控向 agent 发 `req` 后要等同 `id` 的 `resp`。这里维护 `id -> oneshot` 的映射,
//! 并处理三种「等不到回应」的情况:
//!
//!   1. **超时** —— agent 活着但没回(box 卡住、配置巨大)。移除 pending,回 `Timeout`。
//!   2. **连接断开** —— `cancel_agent` 把该 agent 的全部 pending 一次性丢弃。
//!      不这样做的话,调用方会一直等到超时才知道对面早就没了。
//!   3. **发送失败** —— agent 当时就不在线。立刻回 `Offline`,不进 pending。
//!
//! **不做重发。** §5.3 已经论证过:agent 上报的是单调累计值,主控没收到就重算;
//! 命令侧则由 §4.1 的握手补齐机制兜底(离线期间错过的变更在重连时下发)。
//! 在这两层之上再加一层重发,只会让「同一个命令执行两次」变成新的正确性问题。

use crate::cluster::Registry;
use sbx_shared::Envelope;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// 默认超时。
///
/// 30 秒是按 `config.apply` 的最坏情况取的:那条路径要 `box.New()` 校验完整配置、
/// `Close()` 旧 box、`Start()` 新 box(§7.4)。心跳类的短命令远用不到这么久,
/// 但给它们单独调一个更短的超时没有收益——超时只在出问题时才生效。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("agent 不在线")]
    Offline,
    #[error("等待 {method} 回应超时({secs}s)")]
    Timeout { method: String, secs: u64 },
    #[error("连接在等待回应期间断开")]
    Disconnected,
    #[error("agent 返回错误:{0}")]
    Remote(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

struct Pending {
    agent_id: i64,
    tx: oneshot::Sender<Envelope>,
}

#[derive(Default)]
pub struct Rpc {
    pending: Mutex<HashMap<String, Pending>>,
    seq: AtomicU64,
}

impl Rpc {
    pub fn new() -> Self {
        Self::default()
    }

    /// 单调递增的请求 id。
    ///
    /// 用序号而不是 UUID:序号在日志里可读得多(能一眼看出「第 7 个请求」),
    /// 且这里的 id 只需在**本进程本连接**内唯一,不需要全局唯一。
    fn next_id(&self) -> String {
        format!("r{}", self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// 向某 agent 发一条请求并等回应。
    ///
    /// 返回 `Ok(payload)` 时表示对端回了成功;对端回 `error` 时映射成 `RpcError::Remote`
    /// ——调用方不需要自己再检查一遍 `envelope.error`。
    pub async fn call(
        &self,
        registry: &Arc<Mutex<Registry>>,
        agent_id: i64,
        method: &str,
        payload: serde_json::Value,
        timeout_secs: u64,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();

        // 先登记再发送。反过来写会有一个窄窗口:
        // 回应比登记先到,于是 `resolve` 找不到 pending 就把它丢了,调用方等到超时。
        self.pending.lock().await.insert(id.clone(), Pending { agent_id, tx });

        let env = Envelope::req(id.clone(), method, payload);
        let sent = registry.lock().await.send(agent_id, env);
        if !sent {
            // 没发出去就别留着 pending 等超时。
            self.pending.lock().await.remove(&id);
            return Err(RpcError::Offline);
        }

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(resp)) => match resp.error {
                Some(e) => Err(RpcError::Remote(e)),
                None => Ok(resp.payload),
            },
            // oneshot 的发送端被丢弃 = 连接断开时 `cancel_agent` 清了 pending。
            Ok(Err(_)) => Err(RpcError::Disconnected),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout { method: method.to_string(), secs: timeout_secs })
            }
        }
    }

    /// 便利包装:用默认超时。
    pub async fn call_default(
        &self,
        registry: &Arc<Mutex<Registry>>,
        agent_id: i64,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        self.call(registry, agent_id, method, payload, DEFAULT_TIMEOUT_SECS).await
    }

    /// 收到 `resp` 时把它交给等待方。
    ///
    /// 返回 false 表示没有对应的等待者——通常是**已经超时**的请求的迟到回应。
    /// 这不是错误,记 trace 即可;若频繁出现,说明超时设得太短。
    pub async fn resolve(&self, resp: Envelope) -> bool {
        let Some(id) = resp.id.clone() else {
            return false;
        };
        let Some(p) = self.pending.lock().await.remove(&id) else {
            return false;
        };
        // 接收端已被丢弃(调用方自己取消了)时 send 失败,不是问题。
        p.tx.send(resp).is_ok()
    }

    /// 连接断开时清掉该 agent 的全部 pending。
    ///
    /// 丢弃 `oneshot::Sender` 会让等待方立刻拿到 `Disconnected`,
    /// 而不是干等到超时——对 30 秒的默认超时来说,这个差别很明显。
    pub async fn cancel_agent(&self, agent_id: i64) -> usize {
        let mut pending = self.pending.lock().await;
        let doomed: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            pending.remove(id);
        }
        doomed.len()
    }

    /// 当前挂起的 RPC 数量。诊断用(TUI 的 §8 状态栏会读它)。
    #[allow(dead_code)]
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbx_shared::method;

    /// 建一个装了一条连接的 registry,返回它和该连接的接收端。
    fn registry_with_agent(
        agent_id: i64,
    ) -> (Arc<Mutex<Registry>>, tokio::sync::mpsc::UnboundedReceiver<Envelope>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Registry::new();
        r.register(agent_id, tx);
        (Arc::new(Mutex::new(r)), rx)
    }

    #[tokio::test]
    async fn call_returns_the_matching_response() {
        let rpc = Arc::new(Rpc::new());
        let (registry, mut rx) = registry_with_agent(1);

        let rpc2 = rpc.clone();
        let caller = tokio::spawn(async move {
            rpc2.call_default(&registry, 1, method::BOX_STATUS, serde_json::Value::Null).await
        });

        // 扮演 agent:收请求,回同 id 的 resp
        let req = rx.recv().await.unwrap();
        assert_eq!(req.method, method::BOX_STATUS);
        let id = req.id.clone().unwrap();
        rpc.resolve(Envelope::resp_ok(
            id,
            method::BOX_STATUS,
            serde_json::json!({"running": true}),
        ))
        .await;

        let payload = caller.await.unwrap().unwrap();
        assert_eq!(payload["running"], true);
        assert_eq!(rpc.pending_count().await, 0, "完成后不该留下 pending");
    }

    #[tokio::test]
    async fn call_to_offline_agent_fails_immediately_without_pending() {
        let rpc = Rpc::new();
        let registry = Arc::new(Mutex::new(Registry::new())); // 空的
        let err = rpc
            .call_default(&registry, 99, method::BOX_STATUS, serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Offline), "得到: {err}");
        assert_eq!(rpc.pending_count().await, 0, "发送失败不该留 pending 等超时");
    }

    #[tokio::test]
    async fn agent_error_response_maps_to_remote_error() {
        let rpc = Arc::new(Rpc::new());
        let (registry, mut rx) = registry_with_agent(1);

        let rpc2 = rpc.clone();
        let caller = tokio::spawn(async move {
            rpc2.call_default(&registry, 1, method::CONFIG_APPLY, serde_json::json!({})).await
        });

        let id = rx.recv().await.unwrap().id.unwrap();
        // config.apply 失败时回 check 失败原文(§4.2)
        rpc.resolve(Envelope::resp_err(id, method::CONFIG_APPLY, "解析 inbound 失败: 端口冲突"))
            .await;

        let err = caller.await.unwrap().unwrap_err();
        match err {
            RpcError::Remote(m) => assert!(m.contains("端口冲突"), "应保留原文: {m}"),
            other => panic!("应是 Remote,得到: {other}"),
        }
    }

    #[tokio::test]
    async fn timeout_removes_pending() {
        let rpc = Rpc::new();
        let (registry, _rx) = registry_with_agent(1);

        // 超时设 0 秒:立刻超时,不必真等
        let err = rpc
            .call(&registry, 1, method::BOX_STATUS, serde_json::Value::Null, 0)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Timeout { .. }), "得到: {err}");
        assert_eq!(rpc.pending_count().await, 0, "超时后必须清掉 pending,否则会泄漏");
    }

    /// 连接断开时等待方应立刻拿到 Disconnected,而不是干等 30 秒超时。
    #[tokio::test]
    async fn cancel_agent_wakes_waiters_immediately() {
        let rpc = Arc::new(Rpc::new());
        let (registry, mut rx) = registry_with_agent(1);

        let rpc2 = rpc.clone();
        let caller = tokio::spawn(async move {
            rpc2.call(&registry, 1, method::BOX_STATUS, serde_json::Value::Null, 300).await
        });
        rx.recv().await.unwrap(); // 确保请求已发出、pending 已登记

        assert_eq!(rpc.cancel_agent(1).await, 1);
        let err = caller.await.unwrap().unwrap_err();
        assert!(matches!(err, RpcError::Disconnected), "得到: {err}");
    }

    /// `cancel_agent` 只该清掉自己那台的 pending。
    #[tokio::test]
    async fn cancel_agent_does_not_touch_other_agents() {
        let rpc = Arc::new(Rpc::new());
        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Registry::new();
        r.register(1, tx1);
        r.register(2, tx2);
        let registry = Arc::new(Mutex::new(r));

        let (r1, r2) = (rpc.clone(), rpc.clone());
        let reg1 = registry.clone();
        let c1 = tokio::spawn(async move {
            r1.call(&reg1, 1, method::BOX_STATUS, serde_json::Value::Null, 300).await
        });
        let reg2 = registry.clone();
        let c2 = tokio::spawn(async move {
            r2.call(&reg2, 2, method::BOX_STATUS, serde_json::Value::Null, 300).await
        });
        rx1.recv().await.unwrap();
        let id2 = rx2.recv().await.unwrap().id.unwrap();

        assert_eq!(rpc.cancel_agent(1).await, 1, "只该清掉 agent 1 的");
        assert!(matches!(c1.await.unwrap().unwrap_err(), RpcError::Disconnected));

        // agent 2 的请求还活着
        rpc.resolve(Envelope::resp_ok(id2, method::BOX_STATUS, serde_json::json!({"ok": 1}))).await;
        assert!(c2.await.unwrap().is_ok(), "另一台 agent 的 pending 不该被牵连");
    }

    /// 迟到的回应(对应请求已超时)应被安静丢弃,不能 panic。
    #[tokio::test]
    async fn late_response_is_dropped_quietly() {
        let rpc = Rpc::new();
        let accepted =
            rpc.resolve(Envelope::resp_ok("r999", method::BOX_STATUS, serde_json::json!({}))).await;
        assert!(!accepted, "没有等待者时应返回 false");
    }

    /// 没有 id 的信封不该被当成 resp 处理(§4:resp 必须带 id)。
    #[tokio::test]
    async fn response_without_id_is_ignored() {
        let rpc = Rpc::new();
        let mut env = Envelope::resp_ok("x", method::BOX_STATUS, serde_json::json!({}));
        env.id = None;
        assert!(!rpc.resolve(env).await);
    }

    #[tokio::test]
    async fn ids_are_unique_across_calls() {
        let rpc = Rpc::new();
        let (registry, mut rx) = registry_with_agent(1);
        // 两个不等待结果的调用(都会超时,但请求已发出)
        let _ = rpc.call(&registry, 1, method::PING, serde_json::Value::Null, 0).await;
        let _ = rpc.call(&registry, 1, method::PING, serde_json::Value::Null, 0).await;

        let a = rx.recv().await.unwrap().id.unwrap();
        let b = rx.recv().await.unwrap().id.unwrap();
        assert_ne!(a, b, "两次请求的 id 必须不同");
    }
}
