//! 建节点时生成的密钥材料(DESIGN.md §9.1)。
//!
//! 旧项目靠外部进程做这两件事:reality 密钥对 `sing-box generate reality-keypair`,
//! 自签证书 `openssl ecparam` + `openssl req`。sbx 两个都在进程内做:
//!
//!   * 主控机上**没有 sing-box 二进制** —— 内嵌它的是 agent(§0.3 结论一);
//!   * 依赖一个恰好装了 openssl 的运行环境,是那种平时不出事、
//!     出事时报「命令未找到」的隐性依赖。rcgen 已经因为 §1.3 的自签证书进来了,
//!     再用一次不增加依赖面。
//!
//! **生成是幂等的**:已经有值的字段一律不动。这条很重要 —— 重跑一遍
//! 不该让线上节点换掉 reality 私钥,那等于让所有客户端在无提示的情况下失联。

use crate::model::node::{NodeParams, Protocol};
use anyhow::{Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// 保留已有节点默认算法及密钥,只列出当前 sing-box 多用户实现支持的常用算法。
pub const SS_DEFAULT_METHOD: &str = "2022-blake3-aes-128-gcm";
pub const SS_METHODS: &[&str] = &[
    SS_DEFAULT_METHOD,
    "2022-blake3-aes-256-gcm",
    "aes-128-gcm",
    "aes-256-gcm",
    "chacha20-ietf-poly1305",
];

pub fn ss_key_len(method: &str) -> Result<Option<usize>> {
    match method {
        "2022-blake3-aes-128-gcm" => Ok(Some(16)),
        "2022-blake3-aes-256-gcm" => Ok(Some(32)),
        "aes-128-gcm" | "aes-256-gcm" | "chacha20-ietf-poly1305" => Ok(None),
        _ => anyhow::bail!(
            "不支持的 Shadowsocks 多用户算法 {method};可选:{}",
            SS_METHODS.join(" / ")
        ),
    }
}

/// 用户明确切换算法时才换服务端密钥;单纯编辑端口等字段不换密钥。
pub fn set_ss_method(params: &mut NodeParams, method: &str) -> Result<()> {
    ss_key_len(method)?;
    if params.ss_method.as_deref().unwrap_or(SS_DEFAULT_METHOD) != method {
        params.ss_password = None;
    }
    params.ss_method = Some(method.to_string());
    fill(Protocol::Shadowsocks, params)
}

/// 按协议补齐 `params` 里缺的密钥材料。已有的值原样保留。
pub fn fill(proto: Protocol, params: &mut NodeParams) -> Result<()> {
    match proto {
        Protocol::VlessReality => {
            if params.private_key.is_none() || params.public_key.is_none() {
                let (priv_k, pub_k) = reality_keypair();
                params.private_key = Some(priv_k);
                params.public_key = Some(pub_k);
            }
            if params.short_id.is_none() {
                params.short_id = Some(short_id());
            }
            if params.server_name.is_none() {
                // 伪装域名。挑一个握手正常、在多数网络里都通的大站。
                params.server_name = Some("www.apple.com".into());
            }
        }
        Protocol::Shadowsocks => {
            if params.ss_method.is_none() {
                params.ss_method = Some(SS_DEFAULT_METHOD.into());
            }
            let key_len = ss_key_len(params.ss_method.as_deref().unwrap())?;
            if params.ss_password.is_none() {
                params.ss_password = Some(match key_len {
                    Some(16) => random_b64_16(),
                    _ => STANDARD.encode(rand::random::<[u8; 32]>()),
                });
            }
            let password = params.ss_password.as_deref().unwrap();
            if password.is_empty() {
                anyhow::bail!("Shadowsocks 服务端密钥不能为空");
            }
            if let Some(length) = key_len {
                let decoded = STANDARD.decode(password).context("SS2022 密钥不是合法 Base64")?;
                if decoded.len() != length {
                    anyhow::bail!("SS2022 服务端密钥需要 {length} 字节");
                }
            }
        }
        Protocol::VlessWs => {
            if params.path.is_none() {
                params.path = Some("/vless".into());
            }
        }
        Protocol::VmessWs => {
            if params.path.is_none() {
                params.path = Some("/vmess".into());
            }
        }
        Protocol::Trojan | Protocol::Tuic | Protocol::Anytls => {
            if params.server_name.is_none() {
                params.server_name = Some("bing.com".into());
            }
            fill_cert(params)?;
        }
        Protocol::Hysteria2 => {
            // hy2 的 inbound **不需要 server_name**(官方示例里也没有这个字段),
            // 客户端的 sni 由订阅链接决定。证书 CN 用一个占位名即可。
            fill_cert(params)?;
        }
        Protocol::Unknown => anyhow::bail!("未知协议,不生成任何密钥材料"),
    }
    Ok(())
}

fn fill_cert(params: &mut NodeParams) -> Result<()> {
    if params.cert_pem.is_some() && params.key_pem.is_some() {
        return Ok(());
    }
    // 只有一半存在时整套重生成 —— 半套证书握手会报一个与真正原因无关的错
    // (与 tls.rs::ensure_cert 同样的理由)。
    let cn = params.server_name.clone().unwrap_or_else(|| "localhost".into());
    let (cert, key) = self_signed(&cn)?;
    params.cert_pem = Some(cert);
    params.key_pem = Some(key);
    Ok(())
}

/// 生成 reality 用的 X25519 密钥对,返回 `(private_key, public_key)`,
/// 均为 base64url-nopad —— 与 `sing-box generate reality-keypair` 的输出格式逐字节一致
/// (`cmd/sing-box/cmd_generate_wireguard.go:58`)。
///
/// 私钥先按 X25519 的规矩 clamp 再存:sing-box 那边是 `wgtypes.GeneratePrivateKey()`
/// 生成即 clamp,存未 clamp 的值会让两边算出的公钥对不上,而症状是
/// 「客户端连不上且没有任何有用的错误信息」。
pub fn reality_keypair() -> (String, String) {
    let mut bytes: [u8; 32] = rand::random();
    bytes[0] &= 248;
    bytes[31] &= 127;
    bytes[31] |= 64;

    let secret = x25519_dalek::StaticSecret::from(bytes);
    let public = x25519_dalek::PublicKey::from(&secret);
    (URL_SAFE_NO_PAD.encode(secret.to_bytes()), URL_SAFE_NO_PAD.encode(public.to_bytes()))
}

/// reality 的 short_id:8 个十六进制字符(4 字节)。
///
/// 长度不是随便定的 —— sing-box 要求它是偶数长度的 hex,且不超过 16 字符。
pub fn short_id() -> String {
    let b: [u8; 4] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// base64(16 随机字节),用于 shadowsocks 2022 系列方法的服务端密钥。
pub fn random_b64_16() -> String {
    let b: [u8; 16] = rand::random();
    STANDARD.encode(b)
}

/// 把用户的 UUID 当作 shadowsocks 2022 的 16 字节用户密钥。
///
/// 2022 系列方法要求用户密码是 base64(16B),而 UUID 恰好就是 16 字节 ——
/// 直接拿来用,省掉一个「每用户每节点再存一份 ss 密码」的表。
/// UUID 解析失败时返回全零而不是报错:那种情况说明库里的 uuid 列被写坏了,
/// 让这个用户连不上比让整台 agent 的配置生成失败要好。
pub fn ss_user_password(uuid: &str) -> String {
    let bytes = uuid::Uuid::parse_str(uuid).map(|u| *u.as_bytes()).unwrap_or([0u8; 16]);
    STANDARD.encode(bytes)
}

pub fn ss_user_password_for(method: &str, uuid: &str, password: &str) -> Result<String> {
    Ok(match ss_key_len(method)? {
        // 兼容已有 AES-128 节点的用户密钥。
        Some(16) => ss_user_password(uuid),
        Some(_) => {
            let mut hash = Sha256::new();
            hash.update(b"sbx/ss2022/aes256/user-key\0");
            hash.update(password.as_bytes());
            STANDARD.encode(hash.finalize())
        }
        None => password.to_string(),
    })
}

pub fn ss_client_password(params: &NodeParams, uuid: &str, password: &str) -> Result<String> {
    let method = params.ss_method.as_deref().unwrap_or(SS_DEFAULT_METHOD);
    let user_key = ss_user_password_for(method, uuid, password)?;
    if ss_key_len(method)?.is_some() {
        let server_key = params.ss_password.as_deref().context("缺少 SS2022 服务端密钥")?;
        Ok(format!("{server_key}:{user_key}"))
    } else {
        Ok(user_key)
    }
}

/// 生成一张自签证书,返回 `(cert_pem, key_pem)`。
///
/// rcgen 默认是 ECDSA P-256:比 RSA 小得多,握手也快。证书内容基本没人验 ——
/// 这些协议的客户端要么 `insecure`、要么钉指纹,CN 填什么只影响手工排查时的观感。
pub fn self_signed(cn: &str) -> Result<(String, String)> {
    let certified =
        rcgen::generate_simple_self_signed(vec![cn.to_string()]).context("生成自签证书失败")?;
    Ok((certified.cert.pem(), certified.key_pair.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ss_switches_keys_only_when_the_algorithm_changes() {
        let mut params = NodeParams::default();
        fill(Protocol::Shadowsocks, &mut params).unwrap();
        let old_key = params.ss_password.clone();
        set_ss_method(&mut params, SS_DEFAULT_METHOD).unwrap();
        assert_eq!(
            params.ss_password, old_key,
            "editing defaults must preserve existing credentials"
        );
        set_ss_method(&mut params, "2022-blake3-aes-256-gcm").unwrap();
        assert_eq!(STANDARD.decode(params.ss_password.as_ref().unwrap()).unwrap().len(), 32);
        assert_ne!(params.ss_password, old_key);
        let key = params.ss_password.clone();
        assert!(set_ss_method(&mut params, "2022-blake3-chacha20-poly1305").is_err());
        assert_eq!(params.ss_password, key, "invalid selection must not change credentials");
        for method in SS_METHODS {
            set_ss_method(&mut params, method).unwrap();
            let user_key = ss_user_password_for(
                method,
                "b831381d-6324-4d53-ad4f-8cda48b30811",
                "alice-password",
            )
            .unwrap();
            match ss_key_len(method).unwrap() {
                Some(length) => assert_eq!(STANDARD.decode(user_key).unwrap().len(), length),
                None => assert_eq!(user_key, "alice-password"),
            }
        }
    }

    /// reality 私钥必须是 clamp 过的:低 3 位清零、最高位清零、次高位置一。
    /// 不 clamp 的话公钥算出来与 sing-box 侧不一致,而那个 bug 没有任何报错。
    #[test]
    fn reality_private_key_is_clamped() {
        for _ in 0..50 {
            let (priv_k, pub_k) = reality_keypair();
            let raw = URL_SAFE_NO_PAD.decode(&priv_k).unwrap();
            assert_eq!(raw.len(), 32);
            assert_eq!(raw[0] & 7, 0, "低 3 位必须清零");
            assert_eq!(raw[31] & 128, 0, "最高位必须清零");
            assert_eq!(raw[31] & 64, 64, "次高位必须置一");
            assert_eq!(URL_SAFE_NO_PAD.decode(&pub_k).unwrap().len(), 32);
        }
    }

    /// base64url-nopad:不能出现 `+` `/` `=`,否则贴进 sing-box 配置会解不出来。
    #[test]
    fn reality_keys_use_url_safe_alphabet_without_padding() {
        let (priv_k, pub_k) = reality_keypair();
        for k in [&priv_k, &pub_k] {
            assert!(!k.contains('+') && !k.contains('/') && !k.contains('='), "{k}");
            assert_eq!(k.len(), 43, "32 字节 base64url-nopad 就是 43 个字符");
        }
    }

    #[test]
    fn same_private_key_always_yields_same_public_key() {
        // 同一私钥反复导公钥必须稳定 —— 否则订阅里的 public_key 会和
        // agent 上跑的那把私钥对不上。
        let (priv_k, pub_k) = reality_keypair();
        let raw: [u8; 32] = URL_SAFE_NO_PAD.decode(&priv_k).unwrap().try_into().unwrap();
        let again = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(raw));
        assert_eq!(URL_SAFE_NO_PAD.encode(again.to_bytes()), pub_k);
    }

    #[test]
    fn short_id_is_eight_hex_chars() {
        for _ in 0..20 {
            let s = short_id();
            assert_eq!(s.len(), 8);
            assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        }
    }

    #[test]
    fn ss_password_is_base64_of_sixteen_bytes() {
        let pw = ss_user_password("b831381d-6324-4d53-ad4f-8cda48b30811");
        assert_eq!(STANDARD.decode(&pw).unwrap().len(), 16);
        // 坏 uuid 不该 panic,也不该让整台 agent 的配置生成失败。
        assert_eq!(STANDARD.decode(ss_user_password("not-a-uuid")).unwrap().len(), 16);
    }

    /// **幂等**:已有的密钥材料不能被覆盖。
    /// 覆盖 reality 私钥等于让所有客户端静默失联。
    #[test]
    fn fill_never_overwrites_existing_material() {
        let mut p = NodeParams {
            private_key: Some("KEEP-PRIV".into()),
            public_key: Some("KEEP-PUB".into()),
            short_id: Some("deadbeef".into()),
            server_name: Some("keep.example.com".into()),
            ..Default::default()
        };
        fill(Protocol::VlessReality, &mut p).unwrap();
        assert_eq!(p.private_key.as_deref(), Some("KEEP-PRIV"));
        assert_eq!(p.public_key.as_deref(), Some("KEEP-PUB"));
        assert_eq!(p.short_id.as_deref(), Some("deadbeef"));
        assert_eq!(p.server_name.as_deref(), Some("keep.example.com"));
    }

    #[test]
    fn fill_generates_what_each_protocol_needs() {
        let mut p = NodeParams::default();
        fill(Protocol::VlessReality, &mut p).unwrap();
        assert!(p.private_key.is_some() && p.public_key.is_some() && p.short_id.is_some());
        assert!(p.cert_pem.is_none(), "reality 不用证书");

        let mut p = NodeParams::default();
        fill(Protocol::Shadowsocks, &mut p).unwrap();
        assert_eq!(p.ss_method.as_deref(), Some(SS_DEFAULT_METHOD));
        assert!(p.ss_password.is_some());

        for proto in [Protocol::Trojan, Protocol::Tuic, Protocol::Anytls, Protocol::Hysteria2] {
            let mut p = NodeParams::default();
            fill(proto, &mut p).unwrap();
            let cert = p.cert_pem.as_deref().unwrap_or_default();
            let key = p.key_pem.as_deref().unwrap_or_default();
            assert!(cert.contains("BEGIN CERTIFICATE"), "{proto} 少了证书");
            assert!(key.contains("BEGIN PRIVATE KEY"), "{proto} 少了私钥");
        }

        let mut p = NodeParams::default();
        fill(Protocol::VlessWs, &mut p).unwrap();
        assert_eq!(p.path.as_deref(), Some("/vless"));

        let mut p = NodeParams::default();
        fill(Protocol::VmessWs, &mut p).unwrap();
        assert_eq!(p.path.as_deref(), Some("/vmess"));
    }

    #[test]
    fn unknown_protocol_is_an_error() {
        let mut p = NodeParams::default();
        assert!(fill(Protocol::Unknown, &mut p).is_err());
    }
}
