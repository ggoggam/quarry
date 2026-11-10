//! Saved connections: a list in `connections.json` in Quarry's settings
//! folder, with each password in the system keychain (the macOS Keychain, or
//! the Secret Service on Linux) under the connection's id.
//!
//! The file never holds a password: one typed into a URL is moved to the
//! keychain when the connection is saved. Keychain calls can block (macOS
//! may ask whether Quarry may read an item), so callers make them off the UI
//! thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::connect::Target;

/// The keychain service Quarry's passwords are filed under.
const SERVICE: &str = "dev.ggoggam.quarry";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Stable across renames; the keychain account the password is under.
    pub id: String,
    pub name: String,
    /// What [`Target::parse`] reads, never with a password in it.
    pub target: String,
    /// Whether the keychain holds a password for it.
    #[serde(default)]
    pub password: bool,
}

impl SavedConnection {
    pub fn target(&self) -> anyhow::Result<Target> {
        Target::parse(&self.target)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    connections: Vec<SavedConnection>,
}

/// The saved connections, in the order they were added.
pub struct Store {
    path: Option<PathBuf>,
    connections: Vec<SavedConnection>,
}

impl Store {
    /// The list in Quarry's settings folder. A file that cannot be read is
    /// moved aside to `connections.json.bak` rather than overwritten later.
    pub fn load_default() -> Store {
        match crate::config_dir() {
            Some(dir) => Store::load(dir.join("connections.json")),
            None => Store {
                path: None,
                connections: Vec::new(),
            },
        }
    }

    pub fn load(path: PathBuf) -> Store {
        let connections = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(file) => file.connections,
                Err(err) => {
                    let backup = path.with_extension("json.bak");
                    log::warn!(
                        "{} is not a connection list ({err}); moving it to {}",
                        path.display(),
                        backup.display()
                    );
                    let _ = std::fs::rename(&path, &backup);
                    Vec::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                log::warn!("reading {}: {err}", path.display());
                Vec::new()
            }
        };
        Store {
            path: Some(path),
            connections,
        }
    }

    pub fn all(&self) -> &[SavedConnection] {
        &self.connections
    }

    pub fn get(&self, id: &str) -> Option<&SavedConnection> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// The saved connection for `target`, if one has the same URL or path.
    pub fn find(&self, target: &Target) -> Option<&SavedConnection> {
        let input = target.clone().take_password().0.to_input();
        self.connections.iter().find(|c| c.target == input)
    }

    /// Turn what the user filled in into a connection and what to do with
    /// its password. Nothing changes until [`Store::commit`].
    ///
    /// - `id` is the connection being edited; a new one reuses the id of a
    ///   saved connection with the same target, so connecting twice to the
    ///   same server does not list it twice.
    /// - `password` is the password field; one typed into the URL is used
    ///   when the field is empty. An empty field keeps a saved password.
    /// - `remember` false forgets a saved password.
    pub fn prepare(
        &self,
        id: Option<&str>,
        name: &str,
        input: &str,
        password: &str,
        remember: bool,
    ) -> anyhow::Result<Prepared> {
        let (target, url_password) = Target::parse(input)?.take_password();
        let existing = match id {
            Some(id) => Some(self.get(id).context("that connection was deleted")?),
            None => self.find(&target),
        };
        let password = if password.is_empty() {
            url_password
        } else {
            Some(password.to_owned())
        };
        let had_password = existing.is_some_and(|c| c.password);
        let secret = match password {
            _ if !target.is_server() || !remember => {
                if had_password {
                    Secret::Delete
                } else {
                    Secret::None
                }
            }
            Some(password) => Secret::Set(password),
            None if had_password => Secret::Keep,
            None => Secret::None,
        };
        let name = match name.trim() {
            "" => existing
                .map(|c| c.name.clone())
                .unwrap_or_else(|| target.default_name()),
            name => name.to_owned(),
        };
        let id = existing.map_or_else(|| self.new_id(), |c| c.id.clone());
        Ok(Prepared {
            connection: SavedConnection {
                id,
                name,
                target: target.to_input(),
                password: matches!(secret, Secret::Set(_) | Secret::Keep),
            },
            secret,
        })
    }

    /// Add or replace `connection` and write the list.
    pub fn commit(&mut self, connection: SavedConnection) -> anyhow::Result<()> {
        match self.connections.iter_mut().find(|c| c.id == connection.id) {
            Some(slot) => *slot = connection,
            None => self.connections.push(connection),
        }
        self.write()
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<Option<SavedConnection>> {
        let Some(ix) = self.connections.iter().position(|c| c.id == id) else {
            return Ok(None);
        };
        let removed = self.connections.remove(ix);
        self.write()?;
        Ok(Some(removed))
    }

    fn new_id(&self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut n = nanos;
        loop {
            let id = format!("{n:x}");
            if self.get(&id).is_none() {
                return id;
            }
            n += 1;
        }
    }

    fn write(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            anyhow::bail!("there is no settings folder to save connections in");
        };
        write_atomically(
            path,
            &serde_json::to_vec_pretty(&File {
                version: 1,
                connections: self.connections.clone(),
            })?,
        )
        .with_context(|| format!("saving {}", path.display()))
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// A connection ready to save, and what its password needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub connection: SavedConnection,
    pub secret: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// No password, before or after.
    None,
    /// Keep the password the keychain holds.
    Keep,
    Set(String),
    Delete,
}

impl Prepared {
    /// Bring the keychain in line, and return a new password. A kept one is
    /// not read: [`resolve`] reads it when connecting. Blocks on the keychain.
    pub fn apply(&self, secrets: &dyn Secrets) -> anyhow::Result<Option<String>> {
        let id = &self.connection.id;
        match &self.secret {
            Secret::None | Secret::Keep => Ok(None),
            Secret::Set(password) => {
                secrets.set(id, password)?;
                Ok(Some(password.clone()))
            }
            Secret::Delete => {
                secrets.delete(id)?;
                Ok(None)
            }
        }
    }
}

/// The saved connection's target with its password in it. Blocks on the
/// keychain.
pub fn resolve(connection: &SavedConnection, secrets: &dyn Secrets) -> anyhow::Result<Target> {
    let target = connection.target()?;
    if !connection.password {
        return Ok(target);
    }
    let password = secrets
        .get(&connection.id)
        .context("reading the password from the keychain")?;
    Ok(match password {
        Some(password) => target.with_password(&password),
        None => target,
    })
}

/// Open what the connection dialog holds, report the engine and release it
/// answered with, and close it again; nothing is saved. An empty password
/// field falls back to a password in the URL, then to the one `saved` has in
/// the keychain. Blocks on the network and the keychain.
pub fn test(
    input: &str,
    password: &str,
    saved: Option<&SavedConnection>,
    secrets: &dyn Secrets,
) -> anyhow::Result<String> {
    let (target, url_password) = Target::parse(input)?.take_password();
    if let Target::File { path, dialect } = &target
        && !path.exists()
    {
        // Opening it would create it.
        return Ok(format!(
            "No file there yet; connecting creates a new {dialect} database"
        ));
    }
    let password = match (password, url_password) {
        ("", Some(password)) => Some(password),
        ("", None) => match saved {
            Some(c) if c.password => secrets
                .get(&c.id)
                .context("reading the password from the keychain")?,
            _ => None,
        },
        (password, _) => Some(password.to_owned()),
    };
    let target = match password {
        Some(password) => target.with_password(&password),
        None => target,
    };
    let driver = crate::connect::open(&target)?;
    Ok(format!("Connected to {}", driver.flavor().label()))
}

/// Where passwords are kept.
pub trait Secrets: Send + Sync {
    fn get(&self, id: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, id: &str, password: &str) -> anyhow::Result<()>;
    /// Deleting a password that is not there is not an error.
    fn delete(&self, id: &str) -> anyhow::Result<()>;
}

/// The system keychain.
pub struct Keychain;

impl Keychain {
    pub fn shared() -> Arc<dyn Secrets> {
        Arc::new(Keychain)
    }

    fn entry(id: &str) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, id).map_err(|err| anyhow::anyhow!("{err}"))
    }
}

impl Secrets for Keychain {
    fn get(&self, id: &str) -> anyhow::Result<Option<String>> {
        match Self::entry(id)?.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(anyhow::anyhow!("{err}")),
        }
    }

    fn set(&self, id: &str, password: &str) -> anyhow::Result<()> {
        Self::entry(id)?
            .set_password(password)
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    fn delete(&self, id: &str) -> anyhow::Result<()> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(anyhow::anyhow!("{err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>);

    impl Secrets for Memory {
        fn get(&self, id: &str) -> anyhow::Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
        fn set(&self, id: &str, password: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().insert(id.into(), password.into());
            Ok(())
        }
        fn delete(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    fn save(
        store: &mut Store,
        secrets: &Memory,
        id: Option<&str>,
        name: &str,
        input: &str,
        password: &str,
        remember: bool,
    ) -> SavedConnection {
        let prepared = store.prepare(id, name, input, password, remember).unwrap();
        prepared.apply(secrets).unwrap();
        store.commit(prepared.connection.clone()).unwrap();
        prepared.connection
    }

    #[test]
    fn passwords_go_to_the_keychain_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Quarry/connections.json");
        let secrets = Memory::default();
        let mut store = Store::load(path.clone());

        let prod = save(
            &mut store,
            &secrets,
            None,
            "Prod",
            "postgres://alice:s3cret@db:5432/app",
            "",
            true,
        );
        assert_eq!(prod.target, "postgres://alice@db:5432/app");
        assert!(prod.password);
        assert_eq!(secrets.get(&prod.id).unwrap().as_deref(), Some("s3cret"));
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("s3cret"), "{file}");

        // A fresh load reads the same list, and resolving puts the password back.
        let store = Store::load(path);
        assert_eq!(store.all(), std::slice::from_ref(&prod));
        assert_eq!(
            resolve(&store.all()[0], &secrets).unwrap().to_input(),
            "postgres://alice:s3cret@db:5432/app"
        );
    }

    #[test]
    fn editing_keeps_replaces_or_forgets_the_password() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Memory::default();
        let mut store = Store::load(dir.path().join("connections.json"));
        let url = "mysql://root@h:3306/shop";
        let saved = save(&mut store, &secrets, None, "", url, "one", true);
        assert_eq!(saved.name, "root@h:3306/shop");
        let id = Some(saved.id.as_str());

        // An empty field keeps it, and the name stays unless one is typed.
        let kept = save(&mut store, &secrets, id, "", url, "", true);
        assert_eq!(
            (kept.name.as_str(), kept.password),
            ("root@h:3306/shop", true)
        );
        assert_eq!(secrets.get(&saved.id).unwrap().as_deref(), Some("one"));

        let replaced = save(&mut store, &secrets, id, "Shop", url, "two", true);
        assert_eq!(replaced.name, "Shop");
        assert_eq!(secrets.get(&saved.id).unwrap().as_deref(), Some("two"));

        let forgotten = save(&mut store, &secrets, id, "", url, "", false);
        assert!(!forgotten.password);
        assert_eq!(secrets.get(&saved.id).unwrap(), None);
        assert_eq!(store.all().len(), 1);
    }

    #[test]
    fn saving_the_same_target_again_updates_it() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Memory::default();
        let mut store = Store::load(dir.path().join("connections.json"));
        let first = save(
            &mut store,
            &secrets,
            None,
            "A",
            "postgres://a@h/db",
            "",
            true,
        );
        let again = save(
            &mut store,
            &secrets,
            None,
            "",
            "postgres://a:pw@h/db",
            "",
            true,
        );
        assert_eq!(again.id, first.id);
        assert_eq!(again.name, "A");
        assert!(again.password);
        let other = save(
            &mut store,
            &secrets,
            None,
            "",
            "postgres://a@h/other",
            "",
            true,
        );
        assert_ne!(other.id, first.id);
        assert_eq!(store.all().len(), 2);

        assert_eq!(store.remove(&first.id).unwrap().unwrap().name, "A");
        assert_eq!(store.all(), std::slice::from_ref(&other));
    }

    #[test]
    fn files_have_no_password_and_keep_their_engine() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Memory::default();
        let mut store = Store::load(dir.path().join("connections.json"));
        let file = dir.path().join("app.bin");
        let saved = save(
            &mut store,
            &secrets,
            None,
            "",
            &format!("sqlite:{}", file.display()),
            "ignored",
            true,
        );
        assert!(!saved.password);
        assert_eq!(saved.name, "app.bin");
        assert_eq!(
            saved.target().unwrap(),
            Target::File {
                dialect: quarry_core::Dialect::Sqlite,
                path: file
            }
        );
    }

    #[test]
    fn tests_connect_without_saving_or_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Memory::default();
        let missing = dir.path().join("new.sqlite");
        let message = test(missing.to_str().unwrap(), "", None, &secrets).unwrap();
        assert!(message.contains("new SQLite database"), "{message}");
        assert!(!missing.exists());

        let existing = dir.path().join("there.duckdb");
        drop(crate::connect::open(&Target::parse(existing.to_str().unwrap()).unwrap()).unwrap());
        let message = test(existing.to_str().unwrap(), "", None, &secrets).unwrap();
        assert!(message.starts_with("Connected to DuckDB"), "{message}");

        assert!(test("", "", None, &secrets).is_err());
    }

    #[test]
    fn a_broken_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.json");
        std::fs::write(&path, "not json").unwrap();
        let store = Store::load(path.clone());
        assert!(store.all().is_empty());
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("connections.json.bak")).unwrap(),
            "not json"
        );
    }
}
