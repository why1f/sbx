//! agent 连接 token 的生成与校验(DESIGN.md §8.1)。
//!
//! **不要换成 argon2 之类的慢 KDF。** 慢 KDF 是为低熵人类密码设计的;
//! 这里的 token 是 32 字节 `OsRng`(256 位熵),爆破在物理上不可能,慢哈希换不到安全性。
//! 反过来它有实际代价:argon2 每行独立 salt → 无法用 hash 索引 →
//! 校验一个进来的 token 要遍历全表逐行跑 KDF,于是任意**未认证**连接
//! 都能放大成 N 次昂贵 KDF,是一个白送的 DoS 面。

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// token 明文的字节数。32 字节 = 256 位熵。
const TOKEN_BYTES: usize = 32;

/// `token_prefix` 的长度(字符数)。用于在库里索引定位候选行。
pub const PREFIX_LEN: usize = 8;

/// 生成一个新 token(base64url,无 padding)。
///
/// 明文**只在生成时显示一次**,库里只存 `hash()` 与 `prefix_of()`。
pub fn generate() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    // OsRng 直接取操作系统的 CSPRNG。不要换成 thread_rng 之类——
    // 那些是为速度优化的，凭据生成要的是密码学强度。
    rand::rngs::OsRng.fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// sha256(token) 的十六进制小写表示。
pub fn hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    // 手写十六进制而不是引 `hex` crate —— 就这一处用得到。
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 前 `PREFIX_LEN` 个字符,给人识别 + 校验时索引定位。
///
/// 按**字符**而非字节切:token 是 base64url,全是 ASCII,两者等价;
/// 但用 `chars().take()` 在万一收到非 ASCII 输入时也不会 panic
/// (`&token[..8]` 会在多字节字符边界上 panic,而这是未认证输入)。
pub fn prefix_of(token: &str) -> String {
    token.chars().take(PREFIX_LEN).collect()
}

/// **恒定时间**比较两个 hash 字符串。
///
/// 用 `==` 会在第一个不同字节处返回,理论上泄露前缀匹配长度。
/// 这里的 hash 都不是秘密(库被读到就全暴露了),但校验路径保持恒定时间是零成本的好习惯。
pub fn verify(computed_hash: &str, stored_hash: &str) -> bool {
    // 长度不等时直接判否:ConstantTimeEq 要求等长切片。
    // 长度本身不是秘密(sha256 十六进制恒为 64 字符)。
    computed_hash.len() == stored_hash.len()
        && computed_hash.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_url_safe() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b, "两次生成不该相同");
        // 32 字节 base64url 无 padding = 43 字符
        assert_eq!(a.len(), 43, "得到: {a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "必须 URL 安全(要贴进一键安装命令): {a}"
        );
    }

    #[test]
    fn hash_is_stable_sha256_hex() {
        // 已知值:sha256("") 的十六进制
        assert_eq!(hash(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let t = generate();
        assert_eq!(hash(&t), hash(&t), "同一输入必须稳定");
        assert_eq!(hash(&t).len(), 64);
        assert_ne!(hash(&t), hash(&generate()));
    }

    #[test]
    fn verify_accepts_match_and_rejects_everything_else() {
        let h = hash("some-token");
        assert!(verify(&h, &h));
        assert!(!verify(&h, &hash("other-token")));
        assert!(!verify(&h, ""), "长度不等应判否而不是 panic");
        assert!(!verify(&h, &h[..63]), "截断的 hash 应判否");
    }

    /// prefix 必须能从明文算出来,且不能因为奇怪输入 panic
    /// —— 它处理的是**未认证**的连接携带的字符串。
    #[test]
    fn prefix_is_safe_on_hostile_input() {
        assert_eq!(prefix_of("abcdefghijkl").len(), PREFIX_LEN);
        assert_eq!(prefix_of("abc"), "abc", "短于 8 就原样返回");
        assert_eq!(prefix_of(""), "");
        // 多字节字符:不能 panic(这是 `&s[..8]` 会踩的坑)
        let cjk = prefix_of("中文字符串测试用例");
        assert_eq!(cjk.chars().count(), PREFIX_LEN);
    }

    #[test]
    fn prefix_of_a_generated_token_matches_its_own_head() {
        let t = generate();
        assert!(t.starts_with(&prefix_of(&t)));
    }
}
