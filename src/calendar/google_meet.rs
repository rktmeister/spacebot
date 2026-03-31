//! Google Meet space creation for calendar invite links.

use crate::config::GoogleMeetAccessType;

use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Serialize};

const DEFAULT_GOOGLE_MEET_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_MEET_CREATE_SPACE_URL: &str = "https://meet.googleapis.com/v2/spaces";

#[derive(Debug, Clone)]
pub struct GoogleMeetClient {
    client: reqwest::Client,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: String,
    access_type: Option<GoogleMeetAccessType>,
}

impl GoogleMeetClient {
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        token_url: Option<&str>,
        access_type: Option<GoogleMeetAccessType>,
    ) -> anyhow::Result<Self> {
        let token_url = token_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_GOOGLE_MEET_TOKEN_URL);
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(format!("spacebot/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .context("failed to build Google Meet HTTP client")?,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            token_url: token_url.to_string(),
            access_type,
        })
    }

    pub async fn create_space(&self) -> anyhow::Result<String> {
        let access_token = self.refresh_access_token().await?;
        let response = self
            .client
            .post(GOOGLE_MEET_CREATE_SPACE_URL)
            .bearer_auth(&access_token)
            .json(&CreateSpaceRequest {
                config: SpaceConfigRequest {
                    access_type: self
                        .access_type
                        .map(GoogleMeetAccessType::as_google_api_value),
                    entry_point_access: "ALL",
                },
            })
            .send()
            .await
            .context("failed to create Google Meet space")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Google Meet spaces.create failed with {status}: {body}"
            ));
        }

        let created: CreateSpaceResponse = response
            .json()
            .await
            .context("failed to decode Google Meet create-space response")?;
        created
            .meeting_uri
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Google Meet create-space response did not include meetingUri"))
    }

    async fn refresh_access_token(&self) -> anyhow::Result<String> {
        let response = self
            .client
            .post(&self.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", self.refresh_token.as_str()),
            ])
            .send()
            .await
            .context("failed to refresh Google OAuth token for Meet")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Google OAuth token refresh failed with {status}: {body}"
            ));
        }

        let token: GoogleTokenResponse = response
            .json()
            .await
            .context("failed to decode Google OAuth token response")?;
        token
            .access_token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Google OAuth token response did not include access_token"))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSpaceRequest {
    config: SpaceConfigRequest<'static>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpaceConfigRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    access_type: Option<&'a str>,
    entry_point_access: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSpaceResponse {
    meeting_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_space_request_omits_access_type_when_unset() {
        let payload = serde_json::to_value(CreateSpaceRequest {
            config: SpaceConfigRequest {
                access_type: None,
                entry_point_access: "ALL",
            },
        })
        .expect("request should serialize");

        assert_eq!(payload["config"]["entryPointAccess"], "ALL");
        assert!(payload["config"].get("accessType").is_none());
    }

    #[test]
    fn create_space_request_serializes_access_type_when_set() {
        let payload = serde_json::to_value(CreateSpaceRequest {
            config: SpaceConfigRequest {
                access_type: Some("RESTRICTED"),
                entry_point_access: "ALL",
            },
        })
        .expect("request should serialize");

        assert_eq!(payload["config"]["accessType"], "RESTRICTED");
        assert_eq!(payload["config"]["entryPointAccess"], "ALL");
    }
}
