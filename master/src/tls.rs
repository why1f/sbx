//! TLS 证书装配与指纹计算(DESIGN.md §1.3)。
//!
//! 主控自己生成一张自签证书,agent 首次连接时固定它的指纹(TOFU pinning)。
//!
//! **不做 CA 体系、不做证书轮换流程、不做双向 mTLS。** 规模不匹配。
//! 换证书的流程就是删掉这两个文件重启,然后更新各 agent 的 `fingerprint`。
//!
//! 关键设计点:**信任锚定在密钥而非名字上**。agent 侧只校验指纹,不校验 SAN/CN,
//! 所以主控改域名或换 IP 都不需要重签证书。这也是为什么下面的 SAN 生成
//! 可以是「尽力而为」——它填错了也不影响功能。

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// 确保证书与私钥都存在,返回证书指纹(`sha256:<hex>`)。
///
/// 不存在时用 rcgen 生成一张自签证书。反复调用是安全的(幂等):
/// 已存在就只读出来算指纹,不会覆盖。
pub fn ensure_cert(cert_path: &str, key_path: &str, listen: &str) -> Result<String> {
    let cert_exists = Path::new(cert_path).exists();
    let key_exists = Path::new(key_path).exists();

    if !cert_exists || !key_exists {
        // 只有一半存在时也重新生成一整套。
        // 半套证书是个坏状态:拿旧证书配新私钥,握手会报一个与真正原因
        // (另一半文件丢了)毫无关系的错误,排查方向被彻底带偏。
        if cert_exists != key_exists {
            tracing::warn!(
                cert_path,
                key_path,
                "证书与私钥只存在一个,重新生成一整套(旧文件会被覆盖)"
            );
        }
        generate(cert_path, key_path, listen)?;
        tracing::info!(cert_path, key_path, "已生成自签证书");
    }

    fingerprint(cert_path)
}

fn generate(cert_path: &str, key_path: &str, listen: &str) -> Result<()> {
    for p in [cert_path, key_path] {
        if let Some(dir) = Path::new(p).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("创建目录 {} 失败", dir.display()))?;
            }
        }
    }

    let key = rcgen::generate_simple_self_signed(sans_for(listen))
        .context("生成自签证书失败")?;

    std::fs::write(cert_path, key.cert.pem())
        .with_context(|| format!("写证书 {cert_path} 失败"))?;
    std::fs::write(key_path, key.key_pair.serialize_pem())
        .with_context(|| format!("写私钥 {key_path} 失败"))?;

    // 私钥是凭据(§11.3),只有属主可读。
    // Windows 上没有等价的 mode 概念,靠目录 ACL 保护,这里跳过。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置 {key_path} 权限为 0600 失败"))?;
    }

    Ok(())
}

/// 从 `cluster.listen` 推出 SAN 列表。
///
/// **填错了不影响功能** —— agent 只校验指纹(见模块注释)。这里只是让证书
/// 在用普通 TLS 工具(openssl s_client、浏览器)手工排查时看起来正常一些。
fn sans_for(listen: &str) -> Vec<String> {
    let mut sans = vec!["localhost".to_string()];

    // "0.0.0.0:18443" → "0.0.0.0";"[::]:18443" → "::"
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');

    // 通配地址不该进 SAN:它不是任何一台机器的名字。
    if !host.is_empty() && host != "0.0.0.0" && host != "::" && host != "localhost" {
        sans.push(host.to_string());
    }
    sans
}

/// 算证书的 SHA-256 指纹,格式 `sha256:<hex>`。
///
/// 指纹取的是**证书 DER 的摘要**,与 `openssl x509 -fingerprint -sha256` 一致,
/// 所以运维可以用 openssl 独立核对这个值。
pub fn fingerprint(cert_path: &str) -> Result<String> {
    let pem = std::fs::read(cert_path).with_context(|| format!("读证书 {cert_path} 失败"))?;
    let mut reader = std::io::BufReader::new(&pem[..]);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("解析证书 {cert_path} 失败"))?;

    let leaf = certs
        .first()
        .with_context(|| format!("{cert_path} 里没有 CERTIFICATE 块"))?;

    let digest = Sha256::digest(leaf.as_ref());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256:{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_pair() -> (String, String) {
        let dir = std::env::temp_dir().join(format!("sbx-tls-{}", uuid::Uuid::new_v4()));
        (
            dir.join("cert.pem").to_string_lossy().into_owned(),
            dir.join("key.pem").to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn generates_cert_and_key_on_first_call() {
        let (c, k) = tmp_pair();
        let fp = ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();

        assert!(Path::new(&c).exists(), "应生成证书");
        assert!(Path::new(&k).exists(), "应生成私钥,且父目录被自动创建");
        assert!(fp.starts_with("sha256:"), "指纹格式: {fp}");
        // sha256 十六进制 64 字符 + "sha256:" 前缀
        assert_eq!(fp.len(), 71, "得到: {fp}");

        let pem = std::fs::read_to_string(&c).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"), "证书应是 PEM");
        let key = std::fs::read_to_string(&k).unwrap();
        assert!(key.contains("PRIVATE KEY"), "私钥应是 PEM");
    }

    /// 幂等:第二次调用不该覆盖证书,指纹必须不变。
    /// 否则每次重启主控都会换证书,把所有 agent 的 TOFU 固定值作废。
    #[test]
    fn is_idempotent_and_keeps_the_same_fingerprint() {
        let (c, k) = tmp_pair();
        let first = ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();
        let cert_bytes = std::fs::read(&c).unwrap();

        let second = ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();
        assert_eq!(first, second, "重复调用不该改变指纹");
        assert_eq!(cert_bytes, std::fs::read(&c).unwrap(), "证书文件不该被重写");
    }

    /// 只剩一半时重新生成一整套 —— 半套证书是坏状态。
    #[test]
    fn regenerates_when_only_one_half_survives() {
        let (c, k) = tmp_pair();
        let first = ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();

        std::fs::remove_file(&k).unwrap(); // 私钥丢了
        let second = ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();

        assert!(Path::new(&k).exists(), "私钥应被重新生成");
        assert_ne!(first, second, "重新生成后指纹必然变化(需要更新各 agent 的固定值)");
    }

    #[test]
    fn different_certs_have_different_fingerprints() {
        let (c1, k1) = tmp_pair();
        let (c2, k2) = tmp_pair();
        assert_ne!(
            ensure_cert(&c1, &k1, "0.0.0.0:18443").unwrap(),
            ensure_cert(&c2, &k2, "0.0.0.0:18443").unwrap()
        );
    }

    #[test]
    fn fingerprint_of_a_missing_file_is_an_error_not_a_panic() {
        assert!(fingerprint("/nonexistent/nope.pem").is_err());
    }

    #[test]
    fn fingerprint_rejects_a_file_without_certificate_block() {
        let p = std::env::temp_dir().join(format!("sbx-notacert-{}.pem", uuid::Uuid::new_v4()));
        std::fs::write(&p, b"just some text\n").unwrap();
        let err = fingerprint(p.to_string_lossy().as_ref()).unwrap_err().to_string();
        assert!(err.contains("CERTIFICATE"), "错误应说明缺什么: {err}");
    }

    #[test]
    fn wildcard_listen_addresses_are_not_put_in_san() {
        // 通配地址不是任何一台机器的名字
        assert_eq!(sans_for("0.0.0.0:18443"), vec!["localhost"]);
        assert_eq!(sans_for("[::]:18443"), vec!["localhost"]);
        // 具体地址/域名应带上
        assert_eq!(
            sans_for("master.example.com:18443"),
            vec!["localhost", "master.example.com"]
        );
        assert_eq!(sans_for("203.0.113.7:18443"), vec!["localhost", "203.0.113.7"]);
        // localhost 不该重复
        assert_eq!(sans_for("localhost:18443"), vec!["localhost"]);
    }

    /// 私钥权限:unix 上必须是 0600(凭据不该组内/全局可读)。
    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (c, k) = tmp_pair();
        ensure_cert(&c, &k, "0.0.0.0:18443").unwrap();
        let mode = std::fs::metadata(&k).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "私钥权限应为 0600,得到 {:o}", mode & 0o777);
    }
}
