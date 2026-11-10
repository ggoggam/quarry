//! Where the editor tabs live on disk, in Quarry's settings folder:
//!
//! - `editors.json`: the open tabs in strip order, the active one, each
//!   tab's picked SQL flavor, and the unsaved text of files changed since
//!   their last save (written when Quarry quits, so it comes back).
//! - `consoles/<connection>/<name>.sql`: each console's text, in a folder per
//!   connection named after it plus a hash of its key.
//! - `scratches/<name>.sql`: each scratch's text.
//!
//! Every call here touches the disk, so the app makes them off the UI thread,
//! except reading `editors.json` once at launch, as it does
//! `connections.json`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gpui_kit::base::input::Rope;
use quarry_core::{Dialect, Flavor};
use serde::{Deserialize, Serialize};

use super::tabs::TabKind;

const VERSION: u32 = 1;

/// Files larger than this open as an error rather than freeze the editor.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// What `editors.json` holds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Entries a later Quarry wrote that this one cannot read are dropped,
    /// not the whole list.
    #[serde(default, deserialize_with = "lenient")]
    pub tabs: Vec<SavedTab>,
    /// Index into `tabs`.
    #[serde(default)]
    pub active: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    #[serde(flatten)]
    pub kind: TabKind,
    /// The flavor picked for this tab, when it does not follow the
    /// connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor: Option<SavedFlavor>,
    /// A file's text when it had changes not saved to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFlavor {
    pub dialect: String,
    #[serde(default)]
    pub version: String,
}

impl SavedFlavor {
    pub fn of(flavor: &Flavor) -> Self {
        Self {
            dialect: flavor.dialect.id().to_owned(),
            version: flavor.version_text.clone(),
        }
    }

    /// `None` for a dialect this build does not know.
    pub fn flavor(&self) -> Option<Flavor> {
        let dialect = Dialect::ALL.into_iter().find(|d| d.id() == self.dialect)?;
        Some(if self.version.is_empty() {
            Flavor::unversioned(dialect)
        } else {
            // The picker's releases, so a restored pick shows checked.
            dialect
                .releases()
                .iter()
                .find(|r| r.label == self.version)
                .map(|r| Flavor::release(dialect, r))
                .unwrap_or_else(|| Flavor::new(dialect, self.version.clone()))
        })
    }
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SavedTab>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(tab) => Some(tab),
            Err(err) => {
                log::warn!("skipping an editor tab Quarry cannot read: {err}");
                None
            }
        })
        .collect())
}

/// Quarry's editor files, under its settings folder. Without one (no home
/// folder) nothing is kept.
#[derive(Debug, Clone)]
pub struct Store {
    root: Option<PathBuf>,
}

impl Store {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self { root }
    }

    pub fn session_path(&self) -> Option<PathBuf> {
        Some(self.root.as_ref()?.join("editors.json"))
    }

    pub fn scratch_dir(&self) -> Option<PathBuf> {
        Some(self.root.as_ref()?.join("scratches"))
    }

    /// Where a tab's text is: a console's or scratch's file in the settings
    /// folder, or the file a file tab edits.
    pub fn path_of(&self, kind: &TabKind) -> Option<PathBuf> {
        match kind {
            TabKind::Console { connection, name } => Some(
                self.root
                    .as_ref()?
                    .join("consoles")
                    .join(connection_folder(connection))
                    .join(format!("{name}.sql")),
            ),
            TabKind::Scratch { name } => Some(self.scratch_dir()?.join(format!("{name}.sql"))),
            TabKind::File { path } => Some(path.clone()),
        }
    }

    /// The session from the last run; `None` on the first run. A file that
    /// cannot be read is moved aside to `editors.json.bak`.
    pub fn load_session(&self) -> Option<Session> {
        let path = self.session_path()?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                log::warn!("reading {}: {err}", path.display());
                return Some(Session::default());
            }
        };
        match serde_json::from_slice::<Session>(&bytes) {
            Ok(session) => Some(session),
            Err(err) => {
                let backup = path.with_extension("json.bak");
                log::warn!(
                    "{} is not an editor session ({err}); moving it to {}",
                    path.display(),
                    backup.display()
                );
                let _ = std::fs::rename(&path, &backup);
                Some(Session::default())
            }
        }
    }

    pub fn write_session(&self, session: &Session) -> anyhow::Result<()> {
        let Some(path) = self.session_path() else {
            return Ok(());
        };
        let session = Session {
            version: VERSION,
            ..session.clone()
        };
        write_atomically(&path, &serde_json::to_vec_pretty(&session)?)
            .with_context(|| format!("saving {}", path.display()))
    }
}

/// `consoles/` folder for a connection key: a readable part, then a hash so
/// keys that read alike stay apart, e.g. `postgres-alice-db-5432-app-1c9a3f20`.
pub fn connection_folder(key: &str) -> String {
    let mut readable = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            readable.push(ch);
        } else if !readable.ends_with('-') {
            readable.push('-');
        }
        if readable.len() >= 40 {
            break;
        }
    }
    let readable = readable.trim_matches(['-', '.']);
    let readable = if readable.is_empty() {
        "connection"
    } else {
        readable
    };
    format!("{readable}-{:08x}", fnv1a(key.as_bytes()))
}

/// FNV-1a, 32 bits: stable across Rust releases, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &b| {
        (hash ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// A text file's contents; `None` when there is no file. Refuses files over
/// [`MAX_FILE_BYTES`] and files that are not UTF-8.
pub fn read_text(path: &Path) -> anyhow::Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let size = file.metadata()?.len();
    anyhow::ensure!(
        size <= MAX_FILE_BYTES,
        "it is {} MB; Quarry opens SQL files up to {} MB",
        size.div_ceil(1024 * 1024),
        MAX_FILE_BYTES / (1024 * 1024)
    );
    let mut bytes = Vec::with_capacity(size as usize);
    std::io::Read::read_to_end(&mut std::io::BufReader::new(file), &mut bytes)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("it is not UTF-8 text"))
}

/// Save a console's or scratch's text, or remove its file when the text is
/// empty, so empty consoles leave nothing behind.
pub fn write_autosaved(path: &Path, text: &Rope) -> anyhow::Result<()> {
    if text.len() == 0 {
        return remove(path);
    }
    let mut bytes = Vec::with_capacity(text.len());
    text.write_to(&mut bytes)?;
    write_atomically(path, &bytes).with_context(|| format!("saving {}", path.display()))
}

/// Save a user's file in place, so its permissions, owner and any link to
/// it stay as they were.
pub fn write_file(path: &Path, text: &Rope) -> anyhow::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    text.write_to(&mut file)?;
    file.flush()?;
    file.into_inner()
        .map_err(|err| err.into_error())?
        .sync_all()?;
    Ok(())
}

/// Remove a file that may not be there.
pub fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(connection: &str, name: &str) -> TabKind {
        TabKind::Console {
            connection: connection.into(),
            name: name.into(),
        }
    }

    #[test]
    fn sessions_round_trip_and_read_as_plain_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().join("Quarry")));
        assert_eq!(store.load_session(), None, "a first run has no session");
        let session = Session {
            version: VERSION,
            tabs: vec![
                SavedTab {
                    kind: console(":memory:", "console"),
                    flavor: Some(SavedFlavor::of(&Flavor::unversioned(Dialect::Postgres))),
                    draft: None,
                },
                SavedTab {
                    kind: TabKind::File {
                        path: "/work/q.sql".into(),
                    },
                    flavor: None,
                    draft: Some("select 2".into()),
                },
            ],
            active: Some(1),
        };
        store.write_session(&session).unwrap();
        assert_eq!(store.load_session(), Some(session));
        let json = std::fs::read_to_string(store.session_path().unwrap()).unwrap();
        assert!(json.contains(r#""kind": "console""#), "{json}");
        assert!(json.contains(r#""connection": ":memory:""#), "{json}");
        assert!(!json.contains("null"), "absent fields are left out: {json}");
    }

    #[test]
    fn reads_what_a_later_quarry_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().to_owned()));
        std::fs::write(
            store.session_path().unwrap(),
            r#"{"version": 3, "future": true, "active": 0, "tabs": [
                {"kind": "notebook", "name": "n"},
                {"kind": "scratch", "name": "scratch", "pinned": true}
            ]}"#,
        )
        .unwrap();
        let session = store.load_session().unwrap();
        assert_eq!(
            session.tabs,
            [SavedTab {
                kind: TabKind::Scratch {
                    name: "scratch".into()
                },
                flavor: None,
                draft: None
            }]
        );
    }

    #[test]
    fn a_broken_session_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Some(dir.path().to_owned()));
        let path = store.session_path().unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(store.load_session(), Some(Session::default()));
        assert!(!path.exists());
        assert!(dir.path().join("editors.json.bak").exists());
    }

    #[test]
    fn files_go_under_the_settings_folder() {
        let store = Store::new(Some("/cfg".into()));
        let path = store
            .path_of(&console("postgres://alice@db:5432/app", "console_1"))
            .unwrap();
        assert_eq!(
            path,
            Path::new("/cfg/consoles")
                .join(connection_folder("postgres://alice@db:5432/app"))
                .join("console_1.sql")
        );
        assert_eq!(
            store
                .path_of(&TabKind::Scratch {
                    name: "scratch_2".into()
                })
                .unwrap(),
            Path::new("/cfg/scratches/scratch_2.sql")
        );
        assert_eq!(Store::new(None).path_of(&console("a", "console")), None);
    }

    #[test]
    fn connection_folders_are_readable_and_distinct() {
        let folder = connection_folder("postgres://alice@db:5432/app");
        assert!(
            folder.starts_with("postgres-alice-db-5432-app-"),
            "{folder}"
        );
        assert_eq!(folder, connection_folder("postgres://alice@db:5432/app"));
        assert_ne!(
            connection_folder("duckdb:/a/b.db"),
            connection_folder("duckdb:/a-b.db")
        );
        assert!(connection_folder(":memory:").starts_with("memory-"));
        assert!(connection_folder("///").starts_with("connection-"));
        assert!(connection_folder(&"x".repeat(500)).len() < 60);
    }

    #[test]
    fn flavors_come_back_as_picked() {
        let release = &Dialect::Postgres.releases()[0];
        let picked = Flavor::release(Dialect::Postgres, release);
        assert_eq!(SavedFlavor::of(&picked).flavor(), Some(picked));
        let latest = Flavor::unversioned(Dialect::Sqlite);
        assert_eq!(SavedFlavor::of(&latest).flavor(), Some(latest));
        let unknown = SavedFlavor {
            dialect: "oracle".into(),
            version: String::new(),
        };
        assert_eq!(unknown.flavor(), None);
    }

    #[test]
    fn reads_and_writes_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consoles/x/console.sql");
        write_autosaved(&path, &Rope::from("select 1")).unwrap();
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("select 1"));
        // Emptied, the file goes.
        write_autosaved(&path, &Rope::from("")).unwrap();
        assert!(!path.exists());
        assert_eq!(read_text(&path).unwrap(), None);

        let file = dir.path().join("q.sql");
        write_file(&file, &Rope::from("select 2;\n")).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "select 2;\n");
        std::fs::write(&file, b"\xff\xfe").unwrap();
        let err = read_text(&file).unwrap_err().to_string();
        assert!(err.contains("not UTF-8"), "{err}");
        remove(&file).unwrap();
        remove(&file).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saving_a_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("q.sql");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_file(&file, &Rope::from("new")).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }
}
