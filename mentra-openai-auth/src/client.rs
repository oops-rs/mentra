use std::{collections::BTreeMap, net::SocketAddr, time::Duration as StdDuration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::StatusCode;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use url::Url;

pub const DEFAULT_CLIENT_ID: &str = "T19P7LJMcLZgUbhBzA85goHf";
pub const DEFAULT_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const DEFAULT_SCOPE: &str = "openid profile email offline_access";

const CALLBACK_PATH: &str = "/callback";
const CALLBACK_TIMEOUT: StdDuration = StdDuration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum OpenAIOAuthError {
    #[error("http transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("failed to parse url: {0}")]
    Url(#[from] url::ParseError),
    #[error("failed to encode or decode json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to bind loopback callback listener: {0}")]
    Io(#[from] std::io::Error),
    #[error("oauth endpoint returned HTTP {status}: {body}")]
    Http { status: StatusCode, body: String },
    #[error("callback timed out waiting for browser redirect")]
    CallbackTimeout,
    #[error("callback did not include an authorization code")]
    MissingCode,
    #[error("callback state mismatch")]
    StateMismatch,
    #[error("oauth endpoint did not return an API key")]
    MissingApiKey,
    #[error("no stored OAuth tokens found")]
    MissingStoredTokens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAITokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub api_key: Option<String>,
    pub expires_at: OffsetDateTime,
}

impl OpenAITokenSet {
    pub fn is_expired(&self, refresh_skew: Duration) -> bool {
        self.expires_at <= OffsetDateTime::now_utc() + refresh_skew
    }

    pub fn require_api_key(&self) -> Result<&str, OpenAIOAuthError> {
        self.api_key.as_deref().ok_or(OpenAIOAuthError::MissingApiKey)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    expires_in_seconds: i64,
}

impl TokenResponse {
    fn into_tokens(self) -> OpenAITokenSet {
        OpenAITokenSet {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            id_token: self.id_token,
            api_key: self.api_key,
            expires_at: OffsetDateTime::now_utc() + Duration::seconds(self.expires_in_seconds),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAIOAuthClient {
    client: reqwest::Client,
    client_id: String,
    auth_url: Url,
    token_url: Url,
}

impl Default for OpenAIOAuthClient {
    fn default() -> Self {
        Self::new(DEFAULT_CLIENT_ID)
    }
}

impl OpenAIOAuthClient {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .build()
                .expect("Failed to build OpenAI OAuth client"),
            client_id: client_id.into(),
            auth_url: Url::parse(DEFAULT_AUTH_URL).expect("Failed to parse auth url"),
            token_url: Url::parse(DEFAULT_TOKEN_URL).expect("Failed to parse token url"),
        }
    }

    pub async fn start_authorization(&self) -> Result<PendingAuthorization, OpenAIOAuthError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let redirect_addr = listener.local_addr()?;
        let redirect_uri = loopback_redirect_uri(redirect_addr)?;
        let code_verifier = random_base64_url(32);
        let code_challenge = pkce_s256(&code_verifier);
        let state = random_base64_url(32);

        let mut authorize_url = self.auth_url.clone();
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("code_challenge_method", "S256")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("code_challenge", &code_challenge)
            .append_pair("scope", DEFAULT_SCOPE)
            .append_pair("state", &state);

        Ok(PendingAuthorization {
            authorize_url,
            redirect_uri,
            state,
            code_verifier,
            listener,
        })
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &Url,
        code_verifier: &str,
    ) -> Result<OpenAITokenSet, OpenAIOAuthError> {
        let body = self
            .client
            .post(self.token_url.clone())
            .form(&BTreeMap::from([
                ("grant_type", "authorization_code"),
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("code", code),
                ("code_verifier", code_verifier),
            ]))
            .send()
            .await?;

        parse_token_response(body).await
    }

    pub async fn refresh_tokens(
        &self,
        refresh_token: &str,
    ) -> Result<OpenAITokenSet, OpenAIOAuthError> {
        let body = self
            .client
            .post(self.token_url.clone())
            .form(&BTreeMap::from([
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("refresh_token", refresh_token),
            ]))
            .send()
            .await?;

        parse_token_response(body).await
    }
}

pub struct PendingAuthorization {
    authorize_url: Url,
    redirect_uri: Url,
    state: String,
    code_verifier: String,
    listener: TcpListener,
}

impl PendingAuthorization {
    pub fn authorize_url(&self) -> &Url {
        &self.authorize_url
    }

    pub fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    pub async fn complete(self, client: &OpenAIOAuthClient) -> Result<OpenAITokenSet, OpenAIOAuthError> {
        let code = timeout(CALLBACK_TIMEOUT, receive_code(self.listener, &self.state))
            .await
            .map_err(|_| OpenAIOAuthError::CallbackTimeout)??;
        client
            .exchange_code(&code, &self.redirect_uri, &self.code_verifier)
            .await
    }
}

async fn parse_token_response(response: reqwest::Response) -> Result<OpenAITokenSet, OpenAIOAuthError> {
    if !response.status().is_success() {
        return Err(OpenAIOAuthError::Http {
            status: response.status(),
            body: response.text().await.unwrap_or_default(),
        });
    }

    let body = response.json::<TokenResponse>().await?;
    Ok(body.into_tokens())
}

async fn receive_code(listener: TcpListener, expected_state: &str) -> Result<String, OpenAIOAuthError> {
    let (mut stream, _) = listener.accept().await?;
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let callback_url = Url::parse(&format!("http://localhost{path}"))?;
    let params: BTreeMap<_, _> = callback_url.query_pairs().into_owned().collect();

    let response = if params.get("state").is_some_and(|state| state == expected_state) {
        success_response().to_string()
    } else {
        error_response("State mismatch")
    };
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;

    if !params
        .get("state")
        .is_some_and(|state| state == expected_state)
    {
        return Err(OpenAIOAuthError::StateMismatch);
    }

    params
        .get("code")
        .cloned()
        .ok_or(OpenAIOAuthError::MissingCode)
}

fn loopback_redirect_uri(addr: SocketAddr) -> Result<Url, OpenAIOAuthError> {
    Url::parse(&format!("http://{}:{}{CALLBACK_PATH}", addr.ip(), addr.port())).map_err(Into::into)
}

fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()))
}

fn random_base64_url(len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn success_response() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<!doctype html><html><body><h1>Authorization complete</h1><p>You can return to Mentra.</p></body></html>"
}

fn error_response(message: &str) -> String {
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<!doctype html><html><body><h1>Authorization failed</h1><p>{message}</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_expiry_uses_refresh_skew() {
        let tokens = OpenAITokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: None,
            api_key: Some("api".into()),
            expires_at: OffsetDateTime::now_utc() + Duration::seconds(30),
        };

        assert!(tokens.is_expired(Duration::seconds(60)));
        assert!(!tokens.is_expired(Duration::seconds(5)));
    }

    #[test]
    fn pkce_challenge_is_url_safe() {
        let challenge = pkce_s256("test-verifier");
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }
}
