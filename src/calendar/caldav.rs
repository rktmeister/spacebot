//! Minimal CalDAV discovery and sync client.

use crate::calendar::types::SyncedCalendarResource;

use anyhow::{Context as _, anyhow};
use reqwest::Method;
use roxmltree::Document;
use url::Url;

#[derive(Debug, Clone)]
pub struct CalDavCalendar {
    pub href: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CalDavResource {
    pub remote_href: String,
    pub etag: Option<String>,
    pub raw_ics: String,
}

#[derive(Debug, Clone)]
pub struct CalDavSyncDelta {
    pub principal_url: Option<String>,
    pub home_set_url: Option<String>,
    pub calendars: Vec<CalDavCalendar>,
    pub resources: Vec<SyncedCalendarResource>,
    pub deleted_hrefs: Vec<String>,
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub mode: &'static str,
}

#[derive(Debug, Clone)]
pub struct CalDavClient {
    client: reqwest::Client,
    base_url: Url,
    username: String,
    password: String,
}

impl CalDavClient {
    pub fn new(
        base_url: &str,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let normalized = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent(format!("spacebot/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .context("failed to build CalDAV HTTP client")?,
            base_url: Url::parse(&normalized).context("invalid CalDAV base_url")?,
            username: username.into(),
            password: password.into(),
        })
    }

    pub fn resolve_href(&self, href: &str) -> anyhow::Result<String> {
        Ok(self.base_url.join(href)?.to_string())
    }

    pub async fn discover(&self) -> anyhow::Result<CalDavSyncDelta> {
        let principal_response = self
            .dav_xml(
                Method::from_bytes(b"PROPFIND")?,
                self.base_url.as_str(),
                "0",
                r#"
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal />
  </d:prop>
</d:propfind>
"#,
            )
            .await?;

        let principal_url = parse_multistatus_hrefs(&principal_response)
            .into_iter()
            .find_map(|response| response.current_user_principal)
            .map(|href| self.resolve_href(&href))
            .transpose()?;

        let home_set_target = principal_url
            .as_deref()
            .unwrap_or_else(|| self.base_url.as_str());
        let home_response = self
            .dav_xml(
                Method::from_bytes(b"PROPFIND")?,
                home_set_target,
                "0",
                r#"
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set />
    <d:displayname />
  </d:prop>
</d:propfind>
"#,
            )
            .await?;

        let home_set_url = parse_multistatus_hrefs(&home_response)
            .into_iter()
            .find_map(|response| response.calendar_home_set)
            .map(|href| self.resolve_href(&href))
            .transpose()?
            .or(principal_url.clone());

        let Some(home_set_url) = home_set_url else {
            return Err(anyhow!(
                "CalDAV discovery did not return a calendar-home-set"
            ));
        };

        let calendars_response = self
            .dav_xml(
                Method::from_bytes(b"PROPFIND")?,
                &home_set_url,
                "1",
                r#"
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:displayname />
    <c:calendar-description />
    <cs:getctag />
    <d:sync-token />
    <c:calendar-timezone />
    <c:calendar-color />
    <d:resourcetype />
  </d:prop>
</d:propfind>
"#,
            )
            .await?;

        let calendars = parse_multistatus_hrefs(&calendars_response)
            .into_iter()
            .filter(|response| response.is_calendar)
            .map(|response| {
                Ok(CalDavCalendar {
                    href: self.resolve_href(&response.href)?,
                    display_name: response.display_name,
                    description: response.calendar_description,
                    color: response.calendar_color,
                    timezone: response.calendar_timezone,
                    ctag: response.ctag,
                    sync_token: response.sync_token,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(CalDavSyncDelta {
            principal_url,
            home_set_url: Some(home_set_url),
            calendars,
            resources: Vec::new(),
            deleted_hrefs: Vec::new(),
            sync_token: None,
            ctag: None,
            mode: "discovery",
        })
    }

    pub async fn sync_calendar(
        &self,
        calendar_href: &str,
        previous_sync_token: Option<&str>,
    ) -> anyhow::Result<CalDavSyncDelta> {
        let normalized_calendar_href = self.resolve_href(calendar_href)?;
        if let Some(sync_token) = previous_sync_token
            && let Ok(delta) = self
                .sync_collection_delta(&normalized_calendar_href, sync_token)
                .await
        {
            return Ok(delta);
        }

        self.full_calendar_sync(&normalized_calendar_href).await
    }

    pub async fn put_resource(
        &self,
        remote_href: &str,
        raw_ics: &str,
        etag: Option<&str>,
        create_only: bool,
    ) -> anyhow::Result<()> {
        let url = self.resolve_href(remote_href)?;
        let mut request = self
            .client
            .put(url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(raw_ics.to_string());

        if create_only {
            request = request.header("If-None-Match", "*");
        } else if let Some(etag) = etag {
            request = request.header("If-Match", etag);
        }

        let response = request
            .send()
            .await
            .context("failed to PUT calendar resource")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("CalDAV PUT failed with {status}: {body}"));
        }

        Ok(())
    }

    pub async fn delete_resource(
        &self,
        remote_href: &str,
        etag: Option<&str>,
    ) -> anyhow::Result<()> {
        let url = self.resolve_href(remote_href)?;
        let mut request = self
            .client
            .request(Method::DELETE, url)
            .basic_auth(&self.username, Some(&self.password));
        if let Some(etag) = etag {
            request = request.header("If-Match", etag);
        }
        let response = request
            .send()
            .await
            .context("failed to DELETE calendar resource")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("CalDAV DELETE failed with {status}: {body}"));
        }
        Ok(())
    }

    async fn full_calendar_sync(&self, calendar_href: &str) -> anyhow::Result<CalDavSyncDelta> {
        let report = self
            .dav_xml(
                Method::from_bytes(b"REPORT")?,
                calendar_href,
                "1",
                r#"
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag />
    <c:calendar-data />
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT" />
    </c:comp-filter>
  </c:filter>
</c:calendar-query>
"#,
            )
            .await?;

        let resources = parse_calendar_resources(&report, self)?
            .into_iter()
            .map(|resource| -> anyhow::Result<SyncedCalendarResource> {
                Ok(SyncedCalendarResource {
                    remote_href: resource.remote_href.clone(),
                    etag: resource.etag.clone(),
                    events: crate::calendar::ics::parse_calendar_events(&resource.raw_ics)?,
                    raw_ics: resource.raw_ics,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let metadata = self.calendar_metadata(calendar_href).await?;

        Ok(CalDavSyncDelta {
            principal_url: None,
            home_set_url: None,
            calendars: vec![metadata.0],
            resources,
            deleted_hrefs: Vec::new(),
            sync_token: metadata.1,
            ctag: metadata.2,
            mode: "full",
        })
    }

    async fn sync_collection_delta(
        &self,
        calendar_href: &str,
        sync_token: &str,
    ) -> anyhow::Result<CalDavSyncDelta> {
        let report = self
            .dav_xml(
                Method::from_bytes(b"REPORT")?,
                calendar_href,
                "1",
                &format!(
                    r#"
<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:sync-token>{}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop>
    <d:getetag />
    <c:calendar-data />
  </d:prop>
</d:sync-collection>
"#,
                    xml_escape(sync_token)
                ),
            )
            .await?;

        let document = Document::parse(&report).context("failed to parse sync-collection XML")?;
        let new_sync_token = document
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == "sync-token")
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let responses = parse_multistatus_hrefs(&report);
        let mut deleted_hrefs = Vec::new();
        let mut resources = Vec::new();
        for response in responses {
            if response.status_code == Some(404) {
                deleted_hrefs.push(self.resolve_href(&response.href)?);
                continue;
            }
            let Some(calendar_data) = response.calendar_data else {
                continue;
            };
            let remote_href = self.resolve_href(&response.href)?;
            resources.push(SyncedCalendarResource {
                remote_href,
                etag: response.etag,
                events: crate::calendar::ics::parse_calendar_events(&calendar_data)?,
                raw_ics: calendar_data,
            });
        }

        let metadata = self.calendar_metadata(calendar_href).await?;

        Ok(CalDavSyncDelta {
            principal_url: None,
            home_set_url: None,
            calendars: vec![metadata.0],
            resources,
            deleted_hrefs,
            sync_token: new_sync_token.or(metadata.1),
            ctag: metadata.2,
            mode: "incremental",
        })
    }

    async fn calendar_metadata(
        &self,
        calendar_href: &str,
    ) -> anyhow::Result<(CalDavCalendar, Option<String>, Option<String>)> {
        let response = self
            .dav_xml(
                Method::from_bytes(b"PROPFIND")?,
                calendar_href,
                "0",
                r#"
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <d:displayname />
    <c:calendar-description />
    <cs:getctag />
    <d:sync-token />
    <c:calendar-timezone />
    <c:calendar-color />
  </d:prop>
</d:propfind>
"#,
            )
            .await?;

        let response = parse_multistatus_hrefs(&response)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("calendar metadata PROPFIND returned no response"))?;

        Ok((
            CalDavCalendar {
                href: self.resolve_href(&response.href)?,
                display_name: response.display_name,
                description: response.calendar_description,
                color: response.calendar_color,
                timezone: response.calendar_timezone,
                ctag: response.ctag.clone(),
                sync_token: response.sync_token.clone(),
            },
            response.sync_token,
            response.ctag,
        ))
    }

    async fn dav_xml(
        &self,
        method: Method,
        url: &str,
        depth: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        let response = self
            .client
            .request(method, url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Depth", depth)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .context("failed to send CalDAV XML request")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() && status.as_u16() != 207 {
            return Err(anyhow!("CalDAV XML request failed with {status}: {body}"));
        }

        Ok(body)
    }
}

#[derive(Debug, Default)]
struct MultiStatusResponse {
    href: String,
    status_code: Option<u16>,
    current_user_principal: Option<String>,
    calendar_home_set: Option<String>,
    display_name: Option<String>,
    calendar_description: Option<String>,
    calendar_color: Option<String>,
    calendar_timezone: Option<String>,
    sync_token: Option<String>,
    ctag: Option<String>,
    etag: Option<String>,
    calendar_data: Option<String>,
    is_calendar: bool,
}

fn parse_calendar_resources(
    xml: &str,
    client: &CalDavClient,
) -> anyhow::Result<Vec<CalDavResource>> {
    let responses = parse_multistatus_hrefs(xml);
    let mut resources = Vec::new();
    for response in responses {
        let Some(calendar_data) = response.calendar_data else {
            continue;
        };
        resources.push(CalDavResource {
            remote_href: client.resolve_href(&response.href)?,
            etag: response.etag,
            raw_ics: calendar_data,
        });
    }
    Ok(resources)
}

fn parse_multistatus_hrefs(xml: &str) -> Vec<MultiStatusResponse> {
    let document = match Document::parse(xml) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(%error, "failed to parse CalDAV multistatus XML");
            return Vec::new();
        }
    };

    document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "response")
        .filter_map(|response| {
            let href = child_text(&response, "href")?;
            let mut parsed = MultiStatusResponse {
                href,
                ..Default::default()
            };

            for propstat in response
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "propstat")
            {
                let status_code = child_text(&propstat, "status")
                    .and_then(parse_http_status_code)
                    .or(parsed.status_code);
                if status_code == Some(404) {
                    parsed.status_code = Some(404);
                    continue;
                }

                let Some(prop) = propstat
                    .children()
                    .find(|node| node.is_element() && node.tag_name().name() == "prop")
                else {
                    continue;
                };

                for node in prop.children().filter(|node| node.is_element()) {
                    match node.tag_name().name() {
                        "current-user-principal" => {
                            parsed.current_user_principal = child_text(&node, "href");
                        }
                        "calendar-home-set" => {
                            parsed.calendar_home_set = child_text(&node, "href");
                        }
                        "displayname" => {
                            parsed.display_name = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "calendar-description" => {
                            parsed.calendar_description = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "calendar-color" => {
                            parsed.calendar_color = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "calendar-timezone" => {
                            parsed.calendar_timezone = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "sync-token" => {
                            parsed.sync_token = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "getctag" => {
                            parsed.ctag = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "getetag" => {
                            parsed.etag = node
                                .text()
                                .map(str::trim)
                                .map(str::to_string)
                                .filter(|value| !value.is_empty());
                        }
                        "calendar-data" => {
                            parsed.calendar_data = node.text().map(str::to_string);
                        }
                        "resourcetype" => {
                            parsed.is_calendar = node.children().any(|child| {
                                child.is_element() && child.tag_name().name() == "calendar"
                            });
                        }
                        _ => {}
                    }
                }
            }

            Some(parsed)
        })
        .collect()
}

fn child_text(node: &roxmltree::Node<'_, '_>, local_name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == local_name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_http_status_code(status_line: String) -> Option<u16> {
    status_line
        .split_whitespace()
        .find_map(|part| part.parse::<u16>().ok())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
