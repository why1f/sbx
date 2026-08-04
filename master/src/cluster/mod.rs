//! 主控侧的集群管理:WS server、agent registry、rpc 关联、上报入库。
//!
//! 组成:
//!   * `delta` —— §5.2 的 epoch/增量算法
//!   * `token` —— §8.1 的凭据生成与校验
//!   * `registry` —— §4.1 的在线连接登记与驱逐
//!   * `server` —— WebSocket 握手、认证、连接生命周期、消息分发,
//!     以及 §4.1 的握手补齐:比对 `config_revision` / `user_state_revision`,
//!     不一致的分别下发 `config.apply`(经 `service::build_agent_config` 组装)
//!     与 `user.state`。组装失败记 `config_build_failed` 审计但**不断开连接**。
//!   * `rpc` —— 请求/响应关联、超时、断连唤醒
//!   * `ingest` —— stats/sysinfo 上报落库(epoch 与增量走 `delta`)

pub mod delta;
pub mod ingest;
pub mod registry;
pub mod rpc;
pub mod server;
pub mod token;

pub use registry::Registry;
pub use rpc::Rpc;
pub use server::ServerState;
