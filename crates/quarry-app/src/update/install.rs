//! Replacing the installed Quarry with a release's build.
//!
//! The build is downloaded into a hidden folder next to the installation, so
//! the final swap is a rename on one volume, and checked against the
//! release's `SHA256SUMS` before anything is replaced:
//!
//! - macOS: the `.dmg` is mounted, its `Quarry.app` copied out and checked
//!   (same bundle identifier, the release's version, a valid signature from
//!   the same team as the running app), then renamed over the running bundle.
//! - Linux: the tarball is unpacked and its `quarry` renamed over the
//!   running binary.
//!
//! A running process keeps the files it has open, so the swap is safe while
//! Quarry runs; the new version starts on the next launch.

use std::fs::{self, File};
use std::io::{BufWriter, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, anyhow, bail, ensure};
use sha2::{Digest as _, Sha256};
use ureq::Agent;

use super::release::{self, Release};

/// What this Quarry replaces when it updates: the `.app` bundle on macOS,
/// the binary elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    path: PathBuf,
}

impl Installation {
    /// The installation `exe` belongs to, or why Quarry can't update it in
    /// place. Checks that its folder is writable, so it touches the disk.
    pub fn find(exe: &Path) -> Result<Installation, String> {
        let exe = exe
            .canonicalize()
            .map_err(|err| format!("Quarry couldn't find its own executable ({err})."))?;
        let path = if cfg!(target_os = "macos") {
            let app = exe
                .ancestors()
                .nth(3)
                .filter(|app| app.extension().is_some_and(|e| e == "app"))
                .ok_or("Quarry isn't running from an app bundle.")?;
            if app
                .components()
                .any(|c| c.as_os_str() == "AppTranslocation")
            {
                return Err("macOS is running Quarry from a temporary, read-only copy. \
                     Move Quarry to Applications to update it in place."
                    .into());
            }
            app.to_path_buf()
        } else if cfg!(target_os = "linux") {
            exe
        } else {
            return Err("Quarry can't update itself on this platform yet.".into());
        };
        let installation = Installation { path };
        staging_dir(installation.parent()).map_err(|_| {
            format!(
                "Quarry can't write to {}, where it's installed.",
                installation.parent().display()
            )
        })?;
        Ok(installation)
    }

    /// The `.app` bundle or binary, which is also what a restart launches.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("/"))
    }
}

/// How much of the build has been downloaded, read by the UI while the
/// download runs.
#[derive(Debug, Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

impl Progress {
    /// From 0 to 1, once the size is known.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| self.done.load(Ordering::Relaxed) as f32 / total as f32)
    }
}

/// Download `release`'s build for this platform and put it in place of
/// `installation`. Blocks on the network, so run it off the UI thread.
pub fn install(
    installation: &Installation,
    release: &Release,
    agent: &Agent,
    progress: &Arc<Progress>,
) -> anyhow::Result<()> {
    let build = release
        .build()
        .ok_or_else(|| anyhow!("{} has no build for this platform", release.version))?;
    let sums = release
        .asset(release::CHECKSUMS)
        .ok_or_else(|| anyhow!("{} publishes no checksums", release.version))?;
    let sums = download_text(agent, &sums.url).context("downloading the checksums")?;
    let expected = release::checksum(&sums, &build.name)
        .ok_or_else(|| anyhow!("the checksums don't list {}", build.name))?;

    let staging = staging_dir(installation.parent())
        .with_context(|| format!("writing to {}", installation.parent().display()))?;
    let archive = staging.path().join(&build.name);
    progress.total.store(build.size, Ordering::Relaxed);
    download(agent, &build.url, &archive, expected, progress)
        .with_context(|| format!("downloading {}", build.name))?;

    if cfg!(target_os = "macos") {
        replace_bundle(&installation.path, &archive, staging.path(), release)
    } else {
        replace_binary(&installation.path, &archive, staging.path(), &build.name)
    }
}

/// A hidden folder next to the installation, removed when dropped.
fn staging_dir(parent: &Path) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".quarry-update-")
        .tempdir_in(parent)
}

fn download_text(agent: &Agent, url: &str) -> anyhow::Result<String> {
    let mut response = agent.get(url).call()?;
    ensure!(
        response.status().is_success(),
        "the server answered {}",
        response.status()
    );
    Ok(response.body_mut().read_to_string()?)
}

/// Download `url` to `path`, failing unless its SHA-256 is `expected`.
fn download(
    agent: &Agent,
    url: &str,
    path: &Path,
    expected: &str,
    progress: &Progress,
) -> anyhow::Result<()> {
    let mut response = agent.get(url).call()?;
    ensure!(
        response.status().is_success(),
        "the server answered {}",
        response.status()
    );
    if let Some(length) = response.body().content_length() {
        progress.total.store(length, Ordering::Relaxed);
    }
    let mut reader = response.body_mut().as_reader();
    let mut file = BufWriter::new(File::create(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
        progress.done.fetch_add(read as u64, Ordering::Relaxed);
    }
    file.flush()?;
    let actual = hex(&hasher.finalize());
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "the download is corrupt: its SHA-256 is {actual}, the release lists {expected}"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Run a command, failing with its stderr if it fails.
fn run(program: &str, args: &[&std::ffi::OsStr]) -> anyhow::Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A mounted disk image, detached when dropped.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let detach = |force: bool| {
            let mut command = Command::new("hdiutil");
            command.arg("detach").arg(&self.0).arg("-quiet");
            if force {
                command.arg("-force");
            }
            command.status().is_ok_and(|s| s.success())
        };
        if !detach(false) && !detach(true) {
            log::warn!("couldn't detach {}", self.0.display());
        }
    }
}

fn replace_bundle(app: &Path, dmg: &Path, staging: &Path, release: &Release) -> anyhow::Result<()> {
    let mount = staging.join("mount");
    fs::create_dir(&mount)?;
    run(
        "hdiutil",
        &[
            "attach".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-readonly".as_ref(),
            "-quiet".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
            dmg.as_os_str(),
        ],
    )
    .context("opening the disk image")?;
    let mounted = Mounted(mount);
    let new_app = staging.join("Quarry.app");
    run(
        "ditto",
        &[
            mounted.0.join("Quarry.app").as_os_str(),
            new_app.as_os_str(),
        ],
    )
    .context("copying Quarry.app out of the disk image")?;
    drop(mounted);

    let id = |app: &Path| plist_value(app, "CFBundleIdentifier");
    ensure!(
        id(&new_app)? == id(app)?,
        "the download is a different app ({})",
        id(&new_app)?
    );
    let version = plist_value(&new_app, "CFBundleShortVersionString")?;
    ensure!(
        version == release.version.to_string(),
        "the download is version {version}, not {}",
        release.version
    );
    run(
        "codesign",
        &[
            "--verify".as_ref(),
            "--deep".as_ref(),
            "--strict".as_ref(),
            new_app.as_os_str(),
        ],
    )
    .context("checking the download's signature")?;
    // A Developer ID build only updates to one signed by the same team. A
    // local build has no team, so it takes whatever the checksum vouched for.
    if let Some(team) = signing_team(app)? {
        let new_team = signing_team(&new_app)?;
        ensure!(
            new_team.as_deref() == Some(team.as_str()),
            "the download isn't signed by the same developer ({} instead of {team})",
            new_team.as_deref().unwrap_or("no team")
        );
    }

    swap(app, &new_app, &staging.join("Quarry.old.app"))
}

/// Put `new` where `current` is, keeping `current` at `old` (inside the
/// staging folder, so it goes when that does). Puts `current` back if the
/// second rename fails.
fn swap(current: &Path, new: &Path, old: &Path) -> anyhow::Result<()> {
    fs::rename(current, old).with_context(|| format!("moving {} aside", current.display()))?;
    if let Err(err) = fs::rename(new, current) {
        let _ = fs::rename(old, current);
        return Err(err).with_context(|| format!("putting the update at {}", current.display()));
    }
    Ok(())
}

fn plist_value(app: &Path, key: &str) -> anyhow::Result<String> {
    let plist = app.join("Contents/Info.plist");
    let value = run(
        "plutil",
        &[
            "-extract".as_ref(),
            key.as_ref(),
            "raw".as_ref(),
            "-o".as_ref(),
            "-".as_ref(),
            plist.as_os_str(),
        ],
    )
    .with_context(|| format!("reading {key} from {}", plist.display()))?;
    Ok(value.trim().to_owned())
}

/// The team identifier `app` is signed with, or `None` for an ad hoc
/// signature.
fn signing_team(app: &Path) -> anyhow::Result<Option<String>> {
    let output = Command::new("codesign")
        .args(["-d", "--verbose=2"])
        .arg(app)
        .output()
        .context("running codesign")?;
    // codesign prints the details on stderr.
    let details = String::from_utf8_lossy(&output.stderr);
    Ok(team_identifier(&details))
}

fn team_identifier(codesign_details: &str) -> Option<String> {
    codesign_details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_owned)
}

fn replace_binary(exe: &Path, tarball: &Path, staging: &Path, name: &str) -> anyhow::Result<()> {
    run(
        "tar",
        &[
            "-xzf".as_ref(),
            tarball.as_os_str(),
            "-C".as_ref(),
            staging.as_os_str(),
        ],
    )
    .context("unpacking the download")?;
    let folder = name.strip_suffix(".tar.gz").unwrap_or(name);
    let new_exe = staging.join(folder).join("quarry");
    ensure!(new_exe.is_file(), "the download has no {folder}/quarry");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&new_exe, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&new_exe, exe).with_context(|| format!("replacing {}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_signing_team() {
        let signed = "Executable=/Applications/Quarry.app/Contents/MacOS/quarry\n\
            Identifier=dev.ggoggam.quarry\nAuthority=Developer ID Application: X (ABCDE12345)\n\
            TeamIdentifier=ABCDE12345\n";
        assert_eq!(team_identifier(signed).as_deref(), Some("ABCDE12345"));
        let adhoc = "Identifier=quarry\nSignature=adhoc\nTeamIdentifier=not set\n";
        assert_eq!(team_identifier(adhoc), None);
        assert_eq!(team_identifier("code object is not signed at all"), None);
    }

    #[test]
    fn swaps_and_puts_back() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("Quarry.app");
        let new = dir.path().join("new.app");
        let old = dir.path().join("old.app");
        fs::create_dir(&current).unwrap();
        fs::write(current.join("v"), "1").unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(new.join("v"), "2").unwrap();
        swap(&current, &new, &old).unwrap();
        assert_eq!(fs::read_to_string(current.join("v")).unwrap(), "2");
        assert_eq!(fs::read_to_string(old.join("v")).unwrap(), "1");

        // A missing update leaves the installation where it was.
        let old = dir.path().join("old2.app");
        assert!(swap(&current, &dir.path().join("gone.app"), &old).is_err());
        assert_eq!(fs::read_to_string(current.join("v")).unwrap(), "2");
    }

    #[test]
    fn downloads_are_checked() {
        let progress = Progress::default();
        assert_eq!(progress.fraction(), None);
        progress.total.store(4, Ordering::Relaxed);
        progress.done.store(1, Ordering::Relaxed);
        assert_eq!(progress.fraction(), Some(0.25));
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaces_the_binary_from_a_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("quarry");
        fs::write(&exe, "old").unwrap();
        let build = dir.path().join("build/quarry-9.9.9-linux-x86_64");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("quarry"), "new").unwrap();
        let tarball = dir.path().join("quarry-9.9.9-linux-x86_64.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(dir.path().join("build"))
            .arg("quarry-9.9.9-linux-x86_64")
            .status()
            .unwrap();
        assert!(status.success());
        let staging = staging_dir(dir.path()).unwrap();
        replace_binary(
            &exe,
            &tarball,
            staging.path(),
            "quarry-9.9.9-linux-x86_64.tar.gz",
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
    }

    /// Installs the latest release over the installation at
    /// `QUARRY_TEST_UPDATE_INSTALL` (a throwaway `Quarry.app` or `quarry`
    /// binary, which this replaces) from GitHub. Skipped without it.
    #[test]
    fn installs_the_latest_release() {
        let Some(target) = std::env::var_os("QUARRY_TEST_UPDATE_INSTALL") else {
            return;
        };
        let target = PathBuf::from(target);
        let exe = if cfg!(target_os = "macos") {
            target.join("Contents/MacOS/quarry")
        } else {
            target.clone()
        };
        let installation = Installation::find(&exe).unwrap();
        assert_eq!(installation.path(), target.canonicalize().unwrap());
        let agent = release::agent();
        let latest = release::latest(&agent).unwrap();
        let progress = Arc::new(Progress::default());
        install(&installation, &latest, &agent, &progress).unwrap();
        assert_eq!(progress.fraction(), Some(1.0));
        if cfg!(target_os = "macos") {
            assert_eq!(
                plist_value(&target, "CFBundleShortVersionString").unwrap(),
                latest.version.to_string()
            );
        }
        // The staging folder is gone.
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".quarry-update-")
            })
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
