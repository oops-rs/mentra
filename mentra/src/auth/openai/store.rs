use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(target_os = "macos")]
use std::process::Command;

use directories::BaseDirs;

use crate::auth::openai::{OpenAIOAuthError, OpenAITokenSet};

const DEFAULT_KEYCHAIN_SERVICE: &str = "com.mentra.openai";
const DEFAULT_KEYCHAIN_ACCOUNT: &str = "default";
#[cfg(target_os = "macos")]
const KEYCHAIN_NOT_FOUND_EXIT_CODE: i32 = 44;

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<OpenAITokenSet>, OpenAIOAuthError>;
    fn save(&self, tokens: &OpenAITokenSet) -> Result<(), OpenAIOAuthError>;
    fn clear(&self) -> Result<(), OpenAIOAuthError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentTokenStoreKind {
    Auto,
    File,
    Keychain,
}

impl PersistentTokenStoreKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::File => "file",
            Self::Keychain => "keychain",
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryTokenStore {
    state: Arc<Mutex<Option<OpenAITokenSet>>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<OpenAITokenSet>, OpenAIOAuthError> {
        Ok(self
            .state
            .lock()
            .expect("memory token store poisoned")
            .clone())
    }

    fn save(&self, tokens: &OpenAITokenSet) -> Result<(), OpenAIOAuthError> {
        *self.state.lock().expect("memory token store poisoned") = Some(tokens.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), OpenAIOAuthError> {
        *self.state.lock().expect("memory token store poisoned") = None;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> PathBuf {
        let base = BaseDirs::new()
            .map(|dirs| dirs.data_local_dir().to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        base.join("mentra").join("auth").join("openai.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for FileTokenStore {
    fn default() -> Self {
        Self::new(Self::default_path())
    }
}

impl TokenStore for FileTokenStore {
    fn load(&self) -> Result<Option<OpenAITokenSet>, OpenAIOAuthError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(OpenAIOAuthError::Io(error)),
        }
    }

    fn save(&self, tokens: &OpenAITokenSet) -> Result<(), OpenAIOAuthError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;

            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&self.path)?
        };

        #[cfg(not(unix))]
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;

        let payload = serde_json::to_vec_pretty(tokens)?;
        file.write_all(&payload)?;
        file.flush()?;
        Ok(())
    }

    fn clear(&self) -> Result<(), OpenAIOAuthError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(OpenAIOAuthError::Io(error)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeychainTokenStore {
    #[cfg(target_os = "macos")]
    service: String,
    #[cfg(target_os = "macos")]
    account: String,
}

impl KeychainTokenStore {
    #[cfg(target_os = "macos")]
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        let _ = (service.into(), account.into());
        Self {}
    }

    pub fn default_service() -> &'static str {
        DEFAULT_KEYCHAIN_SERVICE
    }

    pub fn default_account() -> &'static str {
        DEFAULT_KEYCHAIN_ACCOUNT
    }
}

impl Default for KeychainTokenStore {
    fn default() -> Self {
        Self::new(Self::default_service(), Self::default_account())
    }
}

impl TokenStore for KeychainTokenStore {
    fn load(&self) -> Result<Option<OpenAITokenSet>, OpenAIOAuthError> {
        #[cfg(target_os = "macos")]
        {
            let output = Command::new("security")
                .args([
                    "find-generic-password",
                    "-a",
                    &self.account,
                    "-s",
                    &self.service,
                    "-w",
                ])
                .output()?;

            if output.status.success() {
                let secret = String::from_utf8_lossy(&output.stdout);
                return Ok(Some(serde_json::from_str(secret.trim())?));
            }

            if output.status.code() == Some(KEYCHAIN_NOT_FOUND_EXIT_CODE) {
                return Ok(None);
            }

            Err(command_error(output, "security"))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = self;
            Err(OpenAIOAuthError::UnsupportedStore("keychain"))
        }
    }

    fn save(&self, tokens: &OpenAITokenSet) -> Result<(), OpenAIOAuthError> {
        #[cfg(target_os = "macos")]
        {
            let payload = serde_json::to_string(tokens)?;
            let output = Command::new("security")
                .args([
                    "add-generic-password",
                    "-U",
                    "-a",
                    &self.account,
                    "-s",
                    &self.service,
                    "-w",
                    &payload,
                ])
                .output()?;

            if output.status.success() {
                return Ok(());
            }

            Err(command_error(output, "security"))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = tokens;
            Err(OpenAIOAuthError::UnsupportedStore("keychain"))
        }
    }

    fn clear(&self) -> Result<(), OpenAIOAuthError> {
        #[cfg(target_os = "macos")]
        {
            let output = Command::new("security")
                .args([
                    "delete-generic-password",
                    "-a",
                    &self.account,
                    "-s",
                    &self.service,
                ])
                .output()?;

            if output.status.success() || output.status.code() == Some(KEYCHAIN_NOT_FOUND_EXIT_CODE)
            {
                return Ok(());
            }

            Err(command_error(output, "security"))
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(OpenAIOAuthError::UnsupportedStore("keychain"))
        }
    }
}

pub fn persistent_token_store(kind: PersistentTokenStoreKind) -> Arc<dyn TokenStore> {
    match selected_store_kind(kind) {
        PersistentTokenStoreKind::File => Arc::new(FileTokenStore::default()),
        PersistentTokenStoreKind::Keychain => Arc::new(KeychainTokenStore::default()),
        PersistentTokenStoreKind::Auto => unreachable!("auto should resolve to a concrete store"),
    }
}

pub fn selected_store_kind(kind: PersistentTokenStoreKind) -> PersistentTokenStoreKind {
    match kind {
        PersistentTokenStoreKind::Auto => {
            if cfg!(target_os = "macos") {
                PersistentTokenStoreKind::Keychain
            } else {
                PersistentTokenStoreKind::File
            }
        }
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn command_error(output: std::process::Output, command: &'static str) -> OpenAIOAuthError {
    OpenAIOAuthError::CredentialCommand {
        command,
        status: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use time::{Duration, OffsetDateTime};

    use super::*;

    #[test]
    fn memory_store_round_trips_tokens() {
        let store = MemoryTokenStore::new();
        let tokens = OpenAITokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: Some("id".into()),
            api_key: Some("api".into()),
            expires_at: OffsetDateTime::now_utc() + Duration::seconds(60),
        };

        store.save(&tokens).expect("save tokens");
        assert_eq!(
            store
                .load()
                .expect("load tokens")
                .expect("missing tokens")
                .api_key,
            Some("api".into())
        );
    }

    #[test]
    fn auto_store_resolves_to_platform_backend() {
        let resolved = selected_store_kind(PersistentTokenStoreKind::Auto);
        if cfg!(target_os = "macos") {
            assert_eq!(resolved, PersistentTokenStoreKind::Keychain);
        } else {
            assert_eq!(resolved, PersistentTokenStoreKind::File);
        }
    }
}
