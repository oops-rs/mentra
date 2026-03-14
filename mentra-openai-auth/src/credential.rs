use std::sync::Arc;

use async_trait::async_trait;
use mentra::provider::openai::OpenAICredentialSource;
use time::Duration;
use tokio::sync::Mutex;

use crate::{
    OpenAIOAuthClient, OpenAIOAuthError, OpenAITokenSet, PersistentTokenStoreKind, TokenStore,
    persistent_token_store,
};

pub struct OpenAIOAuthCredentialSource {
    client: OpenAIOAuthClient,
    tokens: Mutex<OpenAITokenSet>,
    store: Option<Arc<dyn TokenStore>>,
    refresh_skew: Duration,
}

impl OpenAIOAuthCredentialSource {
    pub fn new(client: OpenAIOAuthClient, tokens: OpenAITokenSet) -> Self {
        Self {
            client,
            tokens: Mutex::new(tokens),
            store: None,
            refresh_skew: Duration::seconds(60),
        }
    }

    pub fn with_store(mut self, store: Arc<dyn TokenStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_refresh_skew(mut self, refresh_skew: Duration) -> Self {
        self.refresh_skew = refresh_skew;
        self
    }

    pub fn from_store(
        client: OpenAIOAuthClient,
        store: Arc<dyn TokenStore>,
    ) -> Result<Self, OpenAIOAuthError> {
        let tokens = store.load()?.ok_or(OpenAIOAuthError::MissingStoredTokens)?;
        Ok(Self::new(client, tokens).with_store(store))
    }

    pub fn from_persistent_store(
        client: OpenAIOAuthClient,
        kind: PersistentTokenStoreKind,
    ) -> Result<Self, OpenAIOAuthError> {
        Self::from_store(client, persistent_token_store(kind))
    }

    pub fn from_default_persistent_store(
        client: OpenAIOAuthClient,
    ) -> Result<Self, OpenAIOAuthError> {
        Self::from_persistent_store(client, PersistentTokenStoreKind::Auto)
    }

    async fn current_api_key(&self) -> Result<String, OpenAIOAuthError> {
        let mut tokens = self.tokens.lock().await;
        if tokens.is_expired(self.refresh_skew) {
            let refreshed = self.client.refresh_tokens(&tokens.refresh_token).await?;
            if let Some(store) = &self.store {
                store.save(&refreshed)?;
            }
            *tokens = refreshed;
        }

        Ok(tokens.require_api_key()?.to_string())
    }
}

#[async_trait]
impl OpenAICredentialSource for OpenAIOAuthCredentialSource {
    async fn api_key(&self) -> Result<String, String> {
        self.current_api_key()
            .await
            .map_err(|error| error.to_string())
    }
}
