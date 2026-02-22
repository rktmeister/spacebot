//! OpenAI ChatGPT Plus OAuth device flow, token exchange, refresh, and storage.

use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const AUTHORIZATION_URL: &str = "https://auth.openai.com/codex/device";
const POLLING_SAFETY_MARGIN_MS: u64 = 3000;
const DEVICE_FLOW_TIMEOUT_SECS: u64 = 5 * 60;

/// Stored OpenAI OAuth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Expiry as Unix timestamp in milliseconds.
    pub expires_at: i64,
    pub account_id: Option<String>,
}

impl OAuthCredentials {
    /// Check if the access token is expired or about to expire (within 5 minutes).
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let buffer = 5 * 60 * 1000;
        now >= self.expires_at - buffer
    }

    /// Refresh the access token and return updated credentials.
    pub async fn refresh(&self) -> Result<Self> {
        let client = reqwest::Client::new();
        let response = client
            .post(OAUTH_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .context("failed to send OpenAI OAuth refresh request")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read OpenAI OAuth refresh response")?;

        if !status.is_success() {
            anyhow::bail!("OpenAI OAuth refresh failed ({}): {}", status, body);
        }

        let token_response: TokenResponse =
            serde_json::from_str(&body).context("failed to parse OpenAI OAuth refresh response")?;

        let account_id = extract_account_id(&token_response).or_else(|| self.account_id.clone());
        let refresh_token = token_response
            .refresh_token
            .unwrap_or_else(|| self.refresh_token.clone());

        Ok(Self {
            access_token: token_response.access_token,
            refresh_token,
            expires_at: chrono::Utc::now().timestamp_millis()
                + token_response.expires_in.unwrap_or(3600) * 1000,
            account_id,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: String,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    chatgpt_account_id: Option<String>,
    organizations: Option<Vec<TokenOrganization>>,
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<TokenOpenAiAuthClaims>,
}

#[derive(Debug, Deserialize)]
struct TokenOrganization {
    id: String,
}

#[derive(Debug, Deserialize)]
struct TokenOpenAiAuthClaims {
    chatgpt_account_id: Option<String>,
}

/// Data needed by the UI to finish OpenAI device auth.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceAuthorization {
    pub device_auth_id: String,
    pub user_code: String,
    pub poll_interval_secs: u64,
    pub authorization_url: String,
}

fn parse_jwt_claims(token: &str) -> Option<TokenClaims> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<TokenClaims>(&decoded).ok()
}

fn extract_account_id(token_response: &TokenResponse) -> Option<String> {
    let from_claims = |claims: TokenClaims| {
        claims
            .chatgpt_account_id
            .or_else(|| claims.openai_auth.and_then(|auth| auth.chatgpt_account_id))
            .or_else(|| {
                claims
                    .organizations
                    .and_then(|organizations| organizations.into_iter().next())
                    .map(|organization| organization.id)
            })
    };

    token_response
        .id_token
        .as_deref()
        .and_then(parse_jwt_claims)
        .and_then(from_claims)
        .or_else(|| parse_jwt_claims(&token_response.access_token).and_then(from_claims))
}

/// Start OpenAI device authorization and return a user code + poll details.
pub async fn start_device_authorization() -> Result<DeviceAuthorization> {
    let client = reqwest::Client::new();
    let response = client
        .post(DEVICE_CODE_URL)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("spacebot/{}", env!("CARGO_PKG_VERSION")),
        )
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("failed to start OpenAI device authorization")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read OpenAI device authorization response")?;

    if !status.is_success() {
        anyhow::bail!("OpenAI device authorization failed ({}): {}", status, body);
    }

    let parsed: DeviceCodeResponse = serde_json::from_str(&body)
        .context("failed to parse OpenAI device authorization response")?;
    let poll_interval_secs = parsed.interval.parse::<u64>().unwrap_or(5).max(1);

    Ok(DeviceAuthorization {
        device_auth_id: parsed.device_auth_id,
        user_code: parsed.user_code,
        poll_interval_secs,
        authorization_url: AUTHORIZATION_URL.to_string(),
    })
}

async fn poll_device_authorization(
    device_auth_id: &str,
    user_code: &str,
    poll_interval_secs: u64,
) -> Result<DeviceTokenResponse> {
    let client = reqwest::Client::new();
    let start = tokio::time::Instant::now();
    let poll_delay =
        Duration::from_secs(poll_interval_secs) + Duration::from_millis(POLLING_SAFETY_MARGIN_MS);

    loop {
        if start.elapsed() > Duration::from_secs(DEVICE_FLOW_TIMEOUT_SECS) {
            anyhow::bail!("OpenAI device authorization timed out");
        }

        let response = client
            .post(DEVICE_TOKEN_URL)
            .header("Content-Type", "application/json")
            .header(
                "User-Agent",
                format!("spacebot/{}", env!("CARGO_PKG_VERSION")),
            )
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .context("failed to poll OpenAI device authorization")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read OpenAI device authorization poll response")?;

        if status.is_success() {
            let parsed: DeviceTokenResponse = serde_json::from_str(&body)
                .context("failed to parse OpenAI device authorization poll response")?;
            return Ok(parsed);
        }

        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            tokio::time::sleep(poll_delay).await;
            continue;
        }

        anyhow::bail!(
            "OpenAI device authorization polling failed ({}): {}",
            status,
            body
        );
    }
}

/// Complete OpenAI device authorization by polling and exchanging for OAuth tokens.
pub async fn complete_device_authorization(
    device_auth_id: &str,
    user_code: &str,
    poll_interval_secs: u64,
) -> Result<OAuthCredentials> {
    let device_token = poll_device_authorization(device_auth_id, user_code, poll_interval_secs)
        .await
        .context("failed to complete OpenAI device authorization")?;

    let client = reqwest::Client::new();
    let response = client
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", device_token.authorization_code.as_str()),
            ("redirect_uri", DEVICE_REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", device_token.code_verifier.as_str()),
        ])
        .send()
        .await
        .context("failed to exchange OpenAI authorization code for tokens")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read OpenAI token exchange response")?;

    if !status.is_success() {
        anyhow::bail!("OpenAI token exchange failed ({}): {}", status, body);
    }

    let token_response: TokenResponse =
        serde_json::from_str(&body).context("failed to parse OpenAI token exchange response")?;
    let account_id = extract_account_id(&token_response);
    let refresh_token = token_response
        .refresh_token
        .context("OpenAI token response did not include refresh_token")?;

    Ok(OAuthCredentials {
        access_token: token_response.access_token,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp_millis()
            + token_response.expires_in.unwrap_or(3600) * 1000,
        account_id,
    })
}

/// Path to OpenAI OAuth credentials within the instance directory.
pub fn credentials_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("openai_chatgpt_oauth.json")
}

/// Load OpenAI OAuth credentials from disk.
pub fn load_credentials(instance_dir: &Path) -> Result<Option<OAuthCredentials>> {
    let path = credentials_path(instance_dir);
    if !path.exists() {
        return Ok(None);
    }

    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let creds: OAuthCredentials =
        serde_json::from_str(&data).context("failed to parse OpenAI OAuth credentials")?;
    Ok(Some(creds))
}

/// Save OpenAI OAuth credentials to disk with restricted permissions (0600).
pub fn save_credentials(instance_dir: &Path, creds: &OAuthCredentials) -> Result<()> {
    let path = credentials_path(instance_dir);
    let data = serde_json::to_string_pretty(creds)
        .context("failed to serialize OpenAI OAuth credentials")?;

    std::fs::write(&path, &data).with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}
