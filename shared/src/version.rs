/// 线协议版本。信封 `v` 字段的值。
///
/// **不匹配即断开连接**,不做协议协商、不做向下兼容层(DESIGN.md §4)。
/// 任何对 `proto.rs` 里 struct 的**不兼容**改动都必须 +1;
/// 纯新增可选字段(`#[serde(default)]`)不需要动它。
pub const PROTO_VERSION: u32 = 1;
