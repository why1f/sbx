//! agent 自升级的主控侧(DESIGN.md §11.2)。
//!
//! 主控知道三件 agent 不知道的事:**升到哪个版本**、**产物在哪**、**校验和是多少**。
//! 所以 `agent.upgrade` 的 payload 由这里拼好再下发,agent 只负责下载、校验、
//! 替换自己、退出(由 systemd `Restart=always` 拉起新的)。
//!
//! ## 升到哪个版本:主控自己的版本
//!
//! 不去问 GitHub「最新版是什么」,而是直接用 `env!("CARGO_PKG_VERSION")`。
//! §11.1 规定主控与 agent 共用一个版本号,所以「把 agent 升到主控这个版本」
//! 就是「让整个集群同步」——这正是管理员按下那个键时想要的。
//!
//! 反过来(去查 latest)会有一个很难查的后果:主控还是 0.3.7 而 agent 被升到
//! 0.4.0,协议一旦有变化就是**主控自己把集群升挂了**,而且没人会想到去查这里。
//!
//! ## 校验和从哪来
//!
//! 从 release 的 `<产物>.sha256` 现取。agent 侧拿到的 sha256 是**唯一**能挡住
//! 「下到一个坏文件就把自己替换掉」的东西(见 `agent/master/conn.go`
//! 的 `replaceExecutable`),所以取不到校验和时**宁可不升**,而不是下发一个空值。

use anyhow::{Context, Result};

/// 产物下载地址。命名与 `release.yml` 的打包步骤逐字对应 ——
/// 改了那边就要改这里,否则升级会在 agent 侧以 404 告终。
pub fn agent_asset_url(version: &str, arch: &str) -> String {
    format!(
        "https://github.com/why1f/sbx/releases/download/v{version}/sbx-agent-v{version}-linux-{arch}"
    )
}

/// 主控当前版本。agent 会被升到这个版本(见模块头)。
pub fn target_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// agent 报上来的 arch 能不能对应到一个真实的发布产物。
///
/// `runtime.GOARCH` 的取值远不止这两个,而 release 只出这两个(§11.1)。
/// 认不出来就**别下发** —— 下发一个必然 404 的 URL,agent 那边会去下载、失败、
/// 报一句「HTTP 404」,而真正的原因是这台机器的架构根本没有产物。
pub fn normalize_arch(arch: &str) -> Option<&'static str> {
    match arch.trim() {
        "amd64" | "x86_64" => Some("amd64"),
        "arm64" | "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// 取产物的 sha256。
///
/// `.sha256` 文件的格式是 `<hex>  <文件名>`,只取第一段。
/// 取不到就报错,调用方**不许**兜底成空值下发。
pub async fn fetch_sha256(url: &str) -> Result<String> {
    let sum_url = format!("{url}.sha256");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("sbx/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(&sum_url).send().await.with_context(|| format!("取 {sum_url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("取 {sum_url} 失败:HTTP {}", resp.status());
    }
    let body = resp.text().await?;
    let hex = body.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    // 形状先验一遍:64 个十六进制字符。这里放过一个坏值,agent 那边会把它
    // 当成「校验不符」丢掉整个下载 —— 报错指向的方向就完全错了。
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("{sum_url} 里不是一个 sha256:{}", body.trim());
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL 必须和 `release.yml` 打出来的名字逐字一致。
    /// 对不上的表现是 agent 那边一句「HTTP 404」,而没人会想到来看这里。
    #[test]
    fn asset_url_matches_the_release_layout() {
        assert_eq!(
            agent_asset_url("0.3.8", "amd64"),
            "https://github.com/why1f/sbx/releases/download/v0.3.8/sbx-agent-v0.3.8-linux-amd64"
        );
        assert_eq!(
            agent_asset_url("1.0.0", "arm64"),
            "https://github.com/why1f/sbx/releases/download/v1.0.0/sbx-agent-v1.0.0-linux-arm64"
        );
    }

    /// release 只出两个架构。别的一律认不出 —— 认出来就等于下发一个必然 404 的
    /// 地址,而报错会指向网络而不是「这台机器没有产物」。
    #[test]
    fn only_the_two_published_arches_are_accepted() {
        assert_eq!(normalize_arch("amd64"), Some("amd64"));
        assert_eq!(normalize_arch("x86_64"), Some("amd64"));
        assert_eq!(normalize_arch("arm64"), Some("arm64"));
        assert_eq!(normalize_arch("aarch64"), Some("arm64"));
        assert_eq!(normalize_arch(" amd64 "), Some("amd64"), "两边的空白不该让它失败");

        for bad in ["386", "riscv64", "mips64le", "", "amd"] {
            assert_eq!(normalize_arch(bad), None, "{bad} 没有发布产物,不该放过");
        }
    }

    /// 升级目标就是主控自己的版本(§11.1:两边共用一个版本号)。
    #[test]
    fn the_target_is_the_masters_own_version() {
        assert_eq!(target_version(), env!("CARGO_PKG_VERSION"));
    }
}
