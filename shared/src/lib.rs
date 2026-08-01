//! sbx 主控 ↔ agent 的线格式定义。
//!
//! 这个 crate 是**协议的唯一真源**(Rust 侧)。agent 侧手写一份对应的 Go struct,
//! 两边靠 `DESIGN.md` §12 的联调测试保证一致。
//!
//! 改动这里的任何 struct 都要同步改 `agent/proto/`——**没有代码生成,没有编译期检查**。
//! 这是刻意的取舍(避免引 protobuf 与 `build.rs`,见 DESIGN.md §0.3 结论一),
//! 代价就是这份手工同步义务。

pub mod proto;
pub mod version;

pub use proto::*;
pub use version::PROTO_VERSION;
