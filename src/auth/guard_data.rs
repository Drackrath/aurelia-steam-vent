use directories::ProjectDirs;
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fs::{create_dir_all, read_to_string};
use std::io::Write;
use std::path::PathBuf;
use thiserror::Error;

/// Trait for storing steam guard machine tokens
pub trait GuardDataStore {
    type Err: Error;

    /// Store a machine token for an account
    fn store(
        &mut self,
        account: &str,
        machine_token: String,
    ) -> impl Future<Output = Result<(), Self::Err>> + Send;

    /// Retrieve the stored token for an account
    fn load(
        &mut self,
        account: &str,
    ) -> impl Future<Output = Result<Option<String>, Self::Err>> + Send;
}

impl<Store: GuardDataStore> GuardDataStore for &mut Store {
    type Err = Store::Err;

    fn store(
        &mut self,
        account: &str,
        machine_token: String,
    ) -> impl Future<Output = Result<(), Self::Err>> + Send {
        (**self).store(account, machine_token)
    }

    fn load(
        &mut self,
        account: &str,
    ) -> impl Future<Output = Result<Option<String>, Self::Err>> + Send {
        (**self).load(account)
    }
}

/// Error while storing or loading guard data from json file
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FileStoreError {
    /// Error while reading the json file
    #[error("error while reading tokens from {}: {:#}", path.display(), err)]
    Read { err: std::io::Error, path: PathBuf },
    /// Error while writing the json file
    #[error("error while writing tokens to {}: {:#}", path.display(), err)]
    Write { err: std::io::Error, path: PathBuf },
    /// Error when encoding or decoding the tokens
    #[error("error while parsing tokens from {}: {:#}", path.display(), err)]
    Json {
        err: serde_json::error::Error,
        path: PathBuf,
    },
    /// Error while creating the parent directory of the file
    #[error("error while directory {} for tokens: {:#}", path.display(), err)]
    DirCreation { err: std::io::Error, path: PathBuf },
}

/// Store the steam guard data in a json file
pub struct FileGuardDataStore {
    path: PathBuf,
}

impl FileGuardDataStore {
    /// Store the machine token at the provided path
    pub fn new(path: PathBuf) -> Self {
        FileGuardDataStore { path }
    }

    /// Store the machine tokens in the user's cache directory
    ///
    /// This will be
    /// - `$XDG_CACHE_HOME/steam-vent/machine_token.json` (where `$XDG_CACHE_HOME` defaults to `$HOME/.cache`) on Linux
    /// - `$HOME/Library/Caches/steam-vent/steam-vent.steam-vent/machine_token.json` on macOS
    /// - `%LocalAppData%/steam-vent/steam-vent/cache/machine_token.json` on Windows
    pub fn user_cache() -> Self {
        let project_dirs = ProjectDirs::from("", "steam-vent", "steam-vent")
            .expect("user cache not supported on this platform");
        Self::new(project_dirs.cache_dir().join("machine_tokens.json"))
    }

    fn all_tokens(&self) -> Result<HashMap<String, String>, FileStoreError> {
        if !self.path.exists() {
            return Ok(HashMap::default());
        }
        let raw = read_to_string(&self.path).map_err(|err| FileStoreError::Read {
            err,
            path: self.path.clone(),
        })?;
        serde_json::from_str(&raw).map_err(|err| FileStoreError::Json {
            err,
            path: self.path.clone(),
        })
    }

    fn save(&self, tokens: HashMap<String, String>) -> Result<(), FileStoreError> {
        if let Some(parent) = self.path.parent() {
            create_dir_all(parent).map_err(|err| FileStoreError::DirCreation {
                err,
                path: parent.into(),
            })?;
        }

        let raw = serde_json::to_string(&tokens).map_err(|err| FileStoreError::Json {
            err,
            path: self.path.clone(),
        })?;
        write_private(&self.path, raw.as_bytes()).map_err(|err| FileStoreError::Write {
            err,
            path: self.path.clone(),
        })?;
        Ok(())
    }
}

/// Owner-only file, replaced atomically.
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        // Mode only applies on creation.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl GuardDataStore for FileGuardDataStore {
    type Err = FileStoreError;

    async fn store(&mut self, account: &str, machine_token: String) -> Result<(), Self::Err> {
        if !machine_token.is_empty() {
            let mut tokens = self.all_tokens()?;
            tokens.insert(account.into(), machine_token);
            self.save(tokens)
        } else {
            Ok(())
        }
    }

    async fn load(&mut self, account: &str) -> Result<Option<String>, Self::Err> {
        let mut tokens = self.all_tokens()?;
        Ok(tokens.remove(account).filter(|token| !token.is_empty()))
    }
}

/// Don't store guard data
pub struct NullGuardDataStore;

impl GuardDataStore for NullGuardDataStore {
    type Err = Infallible;

    async fn store(&mut self, _account: &str, _machine_token: String) -> Result<(), Self::Err> {
        Ok(())
    }

    async fn load(&mut self, _account: &str) -> Result<Option<String>, Self::Err> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("guard-data-test-{}", std::process::id()));
        dir.join("nested").join("machine_tokens.json")
    }

    #[tokio::test]
    async fn stores_and_loads_tokens() {
        let path = temp_path().with_file_name("roundtrip.json");
        let mut store = FileGuardDataStore::new(path.clone());
        store.store("alice", "token".into()).await.unwrap();
        assert_eq!(store.load("alice").await.unwrap().as_deref(), Some("token"));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path();
        create_dir_all(path.parent().unwrap()).unwrap();
        // Pre-existing world-readable file.
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut store = FileGuardDataStore::new(path.clone());
        store.store("alice", "token".into()).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(path);
    }
}
