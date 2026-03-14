use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use directories::BaseDirs;

use crate::{OpenAIOAuthError, OpenAITokenSet};

pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<OpenAITokenSet>, OpenAIOAuthError>;
    fn save(&self, tokens: &OpenAITokenSet) -> Result<(), OpenAIOAuthError>;
    fn clear(&self) -> Result<(), OpenAIOAuthError>;
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
        Ok(self.state.lock().expect("memory token store poisoned").clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Duration, OffsetDateTime};

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
            store.load().expect("load tokens").expect("missing tokens").api_key,
            Some("api".into())
        );
    }
}
