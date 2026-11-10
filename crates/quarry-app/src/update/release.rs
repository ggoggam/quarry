//! Quarry's GitHub releases: the latest one, its build for this platform and
//! the checksum it publishes for that build.

use std::time::Duration;

use anyhow::{Context as _, bail};
use semver::Version;
use serde::Deserialize;
use ureq::Agent;

const LATEST: &str = "https://api.github.com/repos/ggoggam/quarry/releases/latest";

/// The file every release publishes `sha256sum` lines in.
pub const CHECKSUMS: &str = "SHA256SUMS";

/// The version this build is.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// The HTTP client for the release feed and downloads. A stalled read fails
/// after 30 s instead of hanging a download forever.
pub fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .user_agent(concat!("quarry/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release notes, as GitHub Markdown.
    pub notes: String,
    /// The release's page on GitHub.
    pub page: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

impl Release {
    /// Read a release from the GitHub API's JSON.
    pub fn parse(json: &str) -> anyhow::Result<Release> {
        let api: ApiRelease = serde_json::from_str(json).context("reading the release")?;
        if api.draft || api.prerelease {
            bail!("{} is not a published release", api.tag_name);
        }
        let version = api.tag_name.strip_prefix('v').unwrap_or(&api.tag_name);
        let version = Version::parse(version)
            .with_context(|| format!("the release tag {} is not a version", api.tag_name))?;
        Ok(Release {
            version,
            notes: api.body.unwrap_or_default(),
            page: api.html_url,
            assets: api.assets,
        })
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// The build for this platform, when the release has one.
    pub fn build(&self) -> Option<&Asset> {
        self.asset(&build_name(
            std::env::consts::OS,
            std::env::consts::ARCH,
            &self.version,
        )?)
    }
}

/// The latest published release: GitHub's `latest` skips drafts and
/// prereleases.
pub fn latest(agent: &Agent) -> anyhow::Result<Release> {
    let mut response = agent
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("reaching GitHub")?;
    let status = response.status();
    let body = response
        .body_mut()
        .read_to_string()
        .context("reading GitHub's reply")?;
    match status.as_u16() {
        200 => Release::parse(&body),
        403 | 429 => bail!("GitHub is rate-limiting update checks; try again in an hour"),
        404 => bail!("no release has been published yet"),
        code => bail!("GitHub answered {code}"),
    }
}

/// What the release workflow names each platform's build.
fn build_name(os: &str, arch: &str, version: &Version) -> Option<String> {
    match (os, arch) {
        ("macos", _) => Some(format!("Quarry-{version}-macos-universal.dmg")),
        ("linux", "x86_64" | "aarch64") => Some(format!("quarry-{version}-linux-{arch}.tar.gz")),
        _ => None,
    }
}

/// The SHA-256 that `sha256sum` output lists for `name`, as lowercase hex.
pub fn checksum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        // `sha256sum` marks files it read in binary mode with a `*`.
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(hash)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = r###"{
        "tag_name": "v0.2.0",
        "name": "Quarry 0.2.0",
        "draft": false,
        "prerelease": false,
        "html_url": "https://github.com/ggoggam/quarry/releases/tag/v0.2.0",
        "body": "## What's Changed\n* Autoupdate",
        "assets": [
            {"name": "Quarry-0.2.0-macos-universal.dmg", "size": 31000000,
             "browser_download_url": "https://github.com/ggoggam/quarry/releases/download/v0.2.0/Quarry-0.2.0-macos-universal.dmg"},
            {"name": "quarry-0.2.0-linux-x86_64.tar.gz", "size": 29000000,
             "browser_download_url": "https://github.com/ggoggam/quarry/releases/download/v0.2.0/quarry-0.2.0-linux-x86_64.tar.gz"},
            {"name": "SHA256SUMS", "size": 300,
             "browser_download_url": "https://github.com/ggoggam/quarry/releases/download/v0.2.0/SHA256SUMS"}
        ]
    }"###;

    #[test]
    fn reads_a_release() {
        let release = Release::parse(RELEASE).unwrap();
        assert_eq!(release.version, Version::new(0, 2, 0));
        assert!(release.version > Version::new(0, 1, 0));
        assert_eq!(release.notes, "## What's Changed\n* Autoupdate");
        assert_eq!(release.assets.len(), 3);
        assert_eq!(release.asset(CHECKSUMS).unwrap().size, 300);
    }

    #[test]
    fn refuses_drafts_prereleases_and_odd_tags() {
        let draft = RELEASE.replace(r#""draft": false"#, r#""draft": true"#);
        assert!(Release::parse(&draft).is_err());
        let pre = RELEASE.replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert!(Release::parse(&pre).is_err());
        let odd = RELEASE.replace("v0.2.0\"", "nightly\"");
        assert!(Release::parse(&odd).is_err());
    }

    #[test]
    fn names_each_platforms_build() {
        let version = Version::new(0, 2, 0);
        assert_eq!(
            build_name("macos", "aarch64", &version).as_deref(),
            Some("Quarry-0.2.0-macos-universal.dmg")
        );
        assert_eq!(
            build_name("linux", "aarch64", &version).as_deref(),
            Some("quarry-0.2.0-linux-aarch64.tar.gz")
        );
        assert_eq!(build_name("windows", "x86_64", &version), None);
        assert_eq!(build_name("linux", "riscv64", &version), None);
    }

    #[test]
    fn finds_checksums() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  Quarry-0.2.0-macos-universal.dmg\n{b} *quarry-0.2.0-linux-x86_64.tar.gz\nnot a line\n"
        );
        assert_eq!(
            checksum(&sums, "Quarry-0.2.0-macos-universal.dmg"),
            Some(a.as_str())
        );
        assert_eq!(
            checksum(&sums, "quarry-0.2.0-linux-x86_64.tar.gz"),
            Some(b.as_str())
        );
        assert_eq!(checksum(&sums, "quarry-0.2.0-linux-aarch64.tar.gz"), None);
        assert_eq!(checksum("abc  x\n", "x"), None);
    }
}
