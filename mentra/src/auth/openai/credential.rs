use std::sync::Arc;

use async_trait::async_trait;
use time::Duration;
use tokio::sync::Mutex;

use crate::{
    auth::openai::{
        OpenAIOAuthClient, OpenAIOAuthError, OpenAITokenSet, PendingAuthorization,
        PersistentTokenStoreKind, TokenStore, persistent_token_store,
    },
    provider::openai::OpenAICredentialSource,
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

    pub async fn from_store_or_authorize<F>(
        client: OpenAIOAuthClient,
        store: Arc<dyn TokenStore>,
        on_pending_authorization: F,
    ) -> Result<Self, OpenAIOAuthError>
    where
        F: FnOnce(&PendingAuthorization),
    {
        match Self::from_store(client.clone(), store.clone()) {
            Ok(source) => Ok(source),
            Err(OpenAIOAuthError::MissingStoredTokens) => {
                let pending = client.start_authorization().await?;
                on_pending_authorization(&pending);
                let tokens = pending.complete(&client).await?;
                store.save(&tokens)?;
                Ok(Self::new(client, tokens).with_store(store))
            }
            Err(error) => Err(error),
        }
    }

    pub async fn from_persistent_store_or_authorize<F>(
        client: OpenAIOAuthClient,
        kind: PersistentTokenStoreKind,
        on_pending_authorization: F,
    ) -> Result<Self, OpenAIOAuthError>
    where
        F: FnOnce(&PendingAuthorization),
    {
        Self::from_store_or_authorize(
            client,
            persistent_token_store(kind),
            on_pending_authorization,
        )
        .await
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
