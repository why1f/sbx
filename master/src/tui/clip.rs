//! 复制到剪贴板(OSC 52)。
//!
//! **为什么不是 `arboard` 之类的原生剪贴板库。** 主控几乎总是跑在一台没有图形界面的
//! Linux 服务器上,而人是从自己的电脑 ssh 进去看这个界面的。原生剪贴板库操作的是
//! **服务器那一侧**的 X11/Wayland 剪贴板 —— 在无头机器上直接报错,即使有也复制到了
//! 一个没人看得见的地方。
//!
//! OSC 52 是终端转义序列:进程把内容发给**终端模拟器**,由终端写进用户本地的剪贴板。
//! 它天然穿过 ssh,因为走的就是那条已经建立的终端通道。
//!
//! 代价是**没有回执**:终端不支持就什么都不会发生,程序这边看不出区别。
//! 所以调用方的提示必须如实说这一点,不能写成「已复制」了事。
//! 常见的不支持场景:很旧的终端;tmux 没开 `set -g set-clipboard on`。

use base64::Engine;
use std::io::Write;

/// 生成 OSC 52 序列。分出来是为了能测 —— 真正写 stdout 的那一步测不了。
pub fn osc52(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    // `c` 是 CLIPBOARD 选区(对应 Ctrl-V);BEL(\x07)结尾比 ST 兼容性更好。
    format!("\x1b]52;c;{b64}\x07")
}

/// 把内容发给终端。返回 `Ok` 只代表**序列写出去了**,不代表终端真的接了 ——
/// 这一点没法知道,所以别在上层把它说成「已复制成功」。
pub fn copy(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(osc52(text).as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64_in_the_right_envelope() {
        let s = osc52("hi");
        assert!(s.starts_with("\x1b]52;c;"), "{s:?}");
        assert!(s.ends_with('\x07'), "{s:?}");
        assert!(s.contains("aGk="), "内容要是标准 base64: {s:?}");
    }

    /// 命令里有单引号、空格、非 ASCII 都不影响 —— base64 之后只剩 ASCII,
    /// 不会有任何字符被终端当成序列的结束符提前截断。
    #[test]
    fn payload_survives_quotes_and_unicode() {
        let text = "curl -fsSL x | SBX_TOKEN='a b\"c' bash  # 中文";
        let s = osc52(text);
        let b64 = s.trim_start_matches("\x1b]52;c;").trim_end_matches('\x07');
        let back = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), text);
        assert!(b64.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
    }
}
