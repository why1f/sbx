//! 主控侧的集群管理:WS server、agent registry、rpc 关联、上报入库。
//!
//! 当前完成的部分:
//!   * `delta` —— §5.2 的 epoch/增量算法
//!   * `token` —— §8.1 的凭据生成与校验
//!   * `registry` —— §4.1 的在线连接登记与驱逐
//!   * `server` —— WebSocket 握手、认证、连接生命周期、消息分发
//!   * `rpc` —— 请求/响应关联、超时、断连唤醒
//!   * `ingest` —— stats/sysinfo 上报落库(epoch 与增量走 `delta`)
//!
//! 尚未完成:§4.1 握手补齐时真正组装并下发 `config.apply` / `user.state`
//! (需要 service 层,§12.1 第 5 步)。

pub mod delta;
pub mod ingest;
pub mod registry;
pub mod rpc;
pub mod server;
pub mod token;

pub use registry::Registry;
pub use rpc::Rpc;
pub use server::ServerState;
