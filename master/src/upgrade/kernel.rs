//! Optional agent builds keep the agent version fixed and select a tested kernel.
use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;

const REPOSITORY: &str = "why1f/sbx";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Prerelease,
}

impl Channel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "稳定版",
            Self::Prerelease => "预发布版",
        }
    }
}

pub fn parse_version(input: &str) -> Result<Version> {
    let input = input.trim();
    let version =
        Version::parse(input.strip_prefix('v').unwrap_or(input)).context("内核版本格式无效")?;
    if !version.build.is_empty() {
        anyhow::bail!("内核版本不接受 build metadata 后缀");
    }
    Ok(version)
}

pub fn tag(agent: &str, kernel: &Version) -> String {
    format!("agent-v{agent}-singbox-v{kernel}")
}

pub fn asset_name(agent: &str, kernel: &Version, arch: &str) -> String {
    format!("sbx-agent-v{agent}-singbox-v{kernel}-linux-{arch}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub arch: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    schema_version: u32,
    repository: String,
    pub agent_version: String,
    pub sing_box_version: String,
    pub channel: Channel,
    source_commit: String,
    release_tag: String,
    protocol_version: u32,
    tests_passed: bool,
    assets: Vec<Asset>,
}

impl Manifest {
    pub fn validate(&self, agent: &str) -> Result<()> {
        if self.schema_version != 1
            || !self.tests_passed
            || self.protocol_version != sbx_shared::PROTO_VERSION
        {
            anyhow::bail!("内核构建清单不兼容或没有通过测试");
        }
        if self.repository != REPOSITORY || self.agent_version != agent {
            anyhow::bail!("内核构建不属于本仓库或与当前主控版本不匹配");
        }
        let kernel = parse_version(&self.sing_box_version)?;
        if kernel.pre.is_empty() != (self.channel == Channel::Stable)
            || self.release_tag != tag(agent, &kernel)
        {
            anyhow::bail!("内核构建通道或标签不匹配");
        }
        if self.source_commit.len() != 40
            || !self.source_commit.bytes().all(|c| c.is_ascii_hexdigit())
        {
            anyhow::bail!("内核构建缺少有效源码提交");
        }
        if self.assets.len() != 2 {
            anyhow::bail!("内核构建缺少架构产物");
        }
        for arch in ["amd64", "arm64"] {
            let matches: Vec<_> = self.assets.iter().filter(|a| a.arch == arch).collect();
            if matches.len() != 1 {
                anyhow::bail!("内核构建架构重复或缺失:{arch}");
            }
            let asset = matches[0];
            let url = format!(
                "https://github.com/{REPOSITORY}/releases/download/{}/{}",
                self.release_tag,
                asset_name(agent, &kernel, arch)
            );
            if asset.url != url
                || asset.sha256.len() != 64
                || !asset.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            {
                anyhow::bail!("内核构建下载地址或校验和无效");
            }
        }
        Ok(())
    }

    pub fn asset(&self, arch: &str) -> Result<&Asset> {
        self.assets.iter().find(|a| a.arch == arch).context("该内核构建没有对应架构")
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn latest(releases: &[Release], agent: &str, channel: Channel) -> Option<(Version, String)> {
    let prefix = format!("agent-v{agent}-singbox-v");
    releases
        .iter()
        .filter(|r| !r.draft && r.prerelease == (channel == Channel::Prerelease))
        .filter_map(|r| {
            let version = parse_version(r.tag_name.strip_prefix(&prefix)?).ok()?;
            if version.pre.is_empty() != (channel == Channel::Stable) {
                return None;
            }
            Some((version, r.tag_name.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
}

/// 查询的是已经编译、测试并发布的变体,不会自动启动 GitHub 构建或升级。
pub async fn resolve(channel: Channel, requested: &str) -> Result<Manifest> {
    let agent = super::target_version();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("sbx/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let (kernel, release_tag) = if requested.trim().is_empty() {
        let mut releases = Vec::new();
        for page in 1..=5 {
            let found: Vec<Release> = client
                .get(format!("https://api.github.com/repos/{REPOSITORY}/releases"))
                .query(&[("per_page", 100), ("page", page)])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let last_page = found.len() < 100;
            releases.extend(found);
            if last_page {
                break;
            }
        }
        latest(&releases, agent, channel).with_context(|| {
            format!(
                "还没有适用于 sbx {agent} 的{}内核构建;请先在 GitHub 手动运行内核构建工作流",
                channel.label()
            )
        })?
    } else {
        let kernel = parse_version(requested)?;
        if kernel.pre.is_empty() != (channel == Channel::Stable) {
            anyhow::bail!("指定内核版本与所选稳定/预发布通道不一致");
        }
        let release_tag = tag(agent, &kernel);
        (kernel, release_tag)
    };
    let url = format!(
        "https://github.com/{REPOSITORY}/releases/download/{release_tag}/kernel-manifest.json"
    );
    let response = client.get(url).send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!(
            "sing-box {kernel} 尚未为 sbx {agent} 构建;请先在 GitHub 手动运行内核构建工作流"
        );
    }
    let response = response.error_for_status()?;
    if response.content_length().is_some_and(|n| n > 1_048_576) {
        anyhow::bail!("内核清单过大");
    }
    let bytes = response.bytes().await?;
    if bytes.len() > 1_048_576 {
        anyhow::bail!("内核清单过大");
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    manifest.validate(agent)?;
    if parse_version(&manifest.sing_box_version)? != kernel || manifest.channel != channel {
        anyhow::bail!("内核清单与选择的版本不一致");
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        serde_json::from_str(include_str!("../../../packaging/testdata/kernel-manifest.json"))
            .unwrap()
    }

    #[test]
    fn manifest_matches_builder_naming_and_rejects_incompatible_artifacts() {
        manifest().validate("0.4.38").unwrap();
        assert!(manifest().validate("0.4.39").is_err());
        let mut m = manifest();
        m.assets[0].url = "https://example.com/binary".into();
        assert!(m.validate("0.4.38").is_err());
        let mut m = manifest();
        m.protocol_version += 1;
        assert!(m.validate("0.4.38").is_err());
        let mut m = manifest();
        m.assets[1] = m.assets[0].clone();
        assert!(m.validate("0.4.38").is_err());
        let mut m = manifest();
        m.tests_passed = false;
        assert!(m.validate("0.4.38").is_err());
    }

    #[test]
    fn latest_means_highest_built_version_in_the_selected_channel() {
        let releases: Vec<Release> = serde_json::from_value(serde_json::json!([
            {"tag_name":"agent-v0.4.38-singbox-v1.14.2","draft":false,"prerelease":false},
            {"tag_name":"agent-v0.4.38-singbox-v1.14.10","draft":false,"prerelease":false},
            {"tag_name":"agent-v0.4.39-singbox-v1.15.0","draft":false,"prerelease":false},
            {"tag_name":"agent-v0.4.38-singbox-v1.15.0-alpha.2","draft":false,"prerelease":true},
            {"tag_name":"agent-v0.4.38-singbox-v1.15.0-alpha.13","draft":false,"prerelease":true},
            {"tag_name":"agent-v0.4.38-singbox-v1.16.0","draft":true,"prerelease":false}
        ]))
        .unwrap();
        assert_eq!(latest(&releases, "0.4.38", Channel::Stable).unwrap().0.to_string(), "1.14.10");
        assert_eq!(
            latest(&releases, "0.4.38", Channel::Prerelease).unwrap().0.to_string(),
            "1.15.0-alpha.13"
        );
        assert!(parse_version("1.14.4/../../x").is_err());
    }
}
