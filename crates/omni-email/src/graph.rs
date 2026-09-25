//! Microsoft Graph [`MailSource`] (plan P4.2, defect E-01).
//!
//! OAuth2 client credentials against the station tenant, then plain REST.
//! Fully async on reqwest — no blocking client, no `spawn_blocking` (E-05).
//!
//! What the mailbox looks like from outside:
//! * processed mail is marked read and moved to `Omni/Processed`;
//! * mail that could not be processed is moved to `Omni/Failed` and left
//!   **unread**, so a human sees it (E-02);
//! * folders are created on first use.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use omni_core::config::GraphConfig;

use crate::mail::{AttachmentMeta, InboundMail};
use crate::source::{MailHealth, MailOutcome, MailSource};

pub const LOGIN_BASE: &str = "https://login.microsoftonline.com";
pub const GRAPH_BASE: &str = "https://graph.microsoft.com";

/// Refresh this long before the token says it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);

/// The server asked us to wait (HTTP 429, or 503 with `Retry-After`).
///
/// Carried in the anyhow chain so the poll loop can honour it instead of
/// hammering a throttled tenant.
#[derive(Debug, Clone, Copy)]
pub struct RetryAfter(pub Duration);

impl std::fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Graph asked to retry after {}s", self.0.as_secs())
    }
}

impl std::error::Error for RetryAfter {}

struct Token {
    value: String,
    valid_until: Instant,
}

pub struct GraphMailSource {
    http: reqwest::Client,
    cfg: GraphConfig,
    login_base: String,
    graph_base: String,
    token: Mutex<Option<Token>>,
    /// Folder path ("Omni/Processed") → Graph folder id, resolved once.
    folders: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for GraphMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No secret, no token: this can end up in a log line.
        f.debug_struct("GraphMailSource")
            .field("tenant_id", &self.cfg.tenant_id)
            .field("client_id", &self.cfg.client_id)
            .field("mailbox", &self.cfg.mailbox)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

#[derive(Deserialize)]
struct GList<T> {
    value: Vec<T>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GAddress {
    #[serde(default)]
    name: String,
    #[serde(default)]
    address: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GRecipient {
    #[serde(default)]
    email_address: GAddress,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GBody {
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GMessage {
    id: String,
    #[serde(default)]
    internet_message_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<GRecipient>,
    #[serde(default)]
    to_recipients: Vec<GRecipient>,
    #[serde(default)]
    cc_recipients: Vec<GRecipient>,
    #[serde(default)]
    received_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    body: Option<GBody>,
    #[serde(default)]
    has_attachments: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GAttachment {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    is_inline: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GFolder {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    unread_item_count: Option<i64>,
}

impl GraphMailSource {
    pub fn new(cfg: GraphConfig) -> Self {
        Self::with_endpoints(cfg, LOGIN_BASE, GRAPH_BASE)
    }

    /// Point at other endpoints: a national cloud, or a mock server in tests.
    pub fn with_endpoints(cfg: GraphConfig, login_base: &str, graph_base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            cfg,
            login_base: login_base.trim_end_matches('/').to_string(),
            graph_base: graph_base.trim_end_matches('/').to_string(),
            token: Mutex::new(None),
            folders: Mutex::new(HashMap::new()),
        }
    }

    /// `{graph}/v1.0/users/{mailbox}/{segments...}`, each segment escaped.
    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = Url::parse(&self.graph_base).context("bad Graph base URL")?;
        {
            let mut p = url.path_segments_mut().map_err(|_| anyhow!("bad Graph base URL"))?;
            p.pop_if_empty().push("v1.0").push("users").push(self.cfg.mailbox.trim());
            for s in segments {
                p.push(s);
            }
        }
        Ok(url)
    }

    async fn token(&self, force_refresh: bool) -> Result<String> {
        let mut guard = self.token.lock().await;
        if !force_refresh {
            if let Some(t) = guard.as_ref() {
                if Instant::now() < t.valid_until {
                    return Ok(t.value.clone());
                }
            }
        }
        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.login_base,
            urlencode(self.cfg.tenant_id.trim())
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.cfg.client_id.trim()),
                ("client_secret", self.cfg.client_secret.as_str()),
                ("scope", "https://graph.microsoft.com/.default"),
            ])
            .send()
            .await
            .context("cannot reach login.microsoftonline.com")?;
        let status = resp.status();
        if !status.is_success() {
            // The body is an AAD error ("AADSTS7000215: Invalid client secret
            // provided"); it names the problem and carries no secret.
            let body = resp.text().await.unwrap_or_default();
            let code = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error_description").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or(body);
            let first_line = code.lines().next().unwrap_or("").chars().take(200).collect::<String>();
            bail!("Graph login failed ({status}): {first_line}");
        }
        let t: TokenResponse = resp.json().await.context("unreadable token response")?;
        let life = Duration::from_secs(t.expires_in).saturating_sub(TOKEN_MARGIN);
        *guard = Some(Token {
            value: t.access_token.clone(),
            valid_until: Instant::now() + life,
        });
        Ok(t.access_token)
    }

    /// Send with a bearer token; on 401 refresh the token once and retry.
    /// 429 and 503-with-Retry-After become a [`RetryAfter`] error.
    async fn send(&self, build: impl Fn(&reqwest::Client) -> RequestBuilder) -> Result<Response> {
        let mut refreshed = false;
        loop {
            let token = self.token(refreshed).await?;
            let resp = build(&self.http).bearer_auth(&token).send().await.context("cannot reach Graph")?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::SERVICE_UNAVAILABLE && resp.headers().contains_key("retry-after"))
            {
                let secs = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(30);
                return Err(anyhow::Error::new(RetryAfter(Duration::from_secs(secs.clamp(1, 600)))));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
                    .unwrap_or(body);
                bail!("Graph returned {status}: {}", msg.chars().take(200).collect::<String>());
            }
            return Ok(resp);
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: Url, prefer_text: bool) -> Result<T> {
        let resp = self
            .send(|c| {
                let r = c.get(url.clone());
                if prefer_text {
                    // Graph converts HTML bodies to text (defect E-06).
                    r.header("Prefer", "outlook.body-content-type=\"text\"")
                } else {
                    r
                }
            })
            .await?;
        resp.json().await.context("unreadable Graph response")
    }

    async fn send_json(&self, method: Method, url: Url, body: serde_json::Value) -> Result<Response> {
        self.send(|c| c.request(method.clone(), url.clone()).json(&body)).await
    }

    /// Folder id for a path like `Omni/Processed`, creating what is missing.
    async fn folder_id(&self, path: &str) -> Result<String> {
        if let Some(id) = self.folders.lock().await.get(path) {
            return Ok(id.clone());
        }
        let mut parent: Option<String> = None;
        for name in path.split('/').map(str::trim).filter(|s| !s.is_empty()) {
            let list_url = match &parent {
                None => self.url(&["mailFolders"])?,
                Some(p) => self.url(&["mailFolders", p, "childFolders"])?,
            };
            let mut filtered = list_url.clone();
            filtered
                .query_pairs_mut()
                .append_pair("$filter", &format!("displayName eq '{}'", name.replace('\'', "''")))
                .append_pair("$select", "id,displayName");
            let found: GList<GFolder> = self.get_json(filtered, false).await?;
            let id = match found.value.into_iter().find(|f| f.display_name.eq_ignore_ascii_case(name)) {
                Some(f) => f.id,
                None => {
                    info!("Graph: creating mail folder '{name}' in {}", self.cfg.mailbox);
                    let created: GFolder = self
                        .send_json(Method::POST, list_url, serde_json::json!({ "displayName": name }))
                        .await?
                        .json()
                        .await
                        .context("unreadable folder-creation response")?;
                    created.id
                }
            };
            parent = Some(id);
        }
        let id = parent.ok_or_else(|| anyhow!("empty folder path"))?;
        self.folders.lock().await.insert(path.to_string(), id.clone());
        Ok(id)
    }

    async fn attachments(&self, message_id: &str) -> Result<Vec<AttachmentMeta>> {
        let mut url = self.url(&["messages", message_id, "attachments"])?;
        url.query_pairs_mut().append_pair("$select", "id,name,contentType,size,isInline");
        let list: GList<GAttachment> = self.get_json(url, false).await?;
        Ok(list
            .value
            .into_iter()
            // Inline parts are signature logos and pasted screenshots.
            .filter(|a| !a.is_inline)
            .map(|a| AttachmentMeta {
                id: a.id,
                name: a.name,
                content_type: a.content_type.unwrap_or_default().to_ascii_lowercase(),
                size: a.size,
            })
            .collect())
    }

    fn to_inbound(&self, m: GMessage, attachments: Vec<AttachmentMeta>) -> InboundMail {
        let from = m.from.unwrap_or_default().email_address;
        let body = m.body.unwrap_or_default();
        let (body_text, body_html) = if body.content_type.eq_ignore_ascii_case("html") {
            (String::new(), Some(body.content))
        } else {
            (body.content, None)
        };
        InboundMail {
            id: m.id,
            internet_message_id: m.internet_message_id.unwrap_or_default(),
            from_address: from.address.trim().to_lowercase(),
            from_name: from.name,
            to: m.to_recipients.into_iter().map(|r| r.email_address.address.to_lowercase()).collect(),
            cc: m.cc_recipients.into_iter().map(|r| r.email_address.address.to_lowercase()).collect(),
            subject: m.subject.unwrap_or_default(),
            received_at: m.received_date_time,
            body_text,
            body_html,
            attachments,
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[async_trait]
impl MailSource for GraphMailSource {
    fn describe(&self) -> String {
        format!("graph {}", self.cfg.mailbox)
    }

    fn is_configured(&self) -> bool {
        self.cfg.is_configured()
    }

    async fn fetch_unprocessed(&self, limit: usize) -> Result<Vec<InboundMail>> {
        let mut url = self.url(&["mailFolders", "Inbox", "messages"])?;
        url.query_pairs_mut()
            // Graph refuses $orderby on a property absent from $filter
            // ("InefficientFilter"), hence the always-true date clause.
            .append_pair("$filter", "receivedDateTime ge 1900-01-01T00:00:00Z and isRead eq false")
            .append_pair("$orderby", "receivedDateTime asc")
            .append_pair("$top", &limit.clamp(1, 50).to_string())
            .append_pair(
                "$select",
                "id,internetMessageId,subject,from,toRecipients,ccRecipients,receivedDateTime,body,hasAttachments",
            );
        let list: GList<GMessage> = self.get_json(url, true).await?;
        let mut out = Vec::with_capacity(list.value.len());
        for m in list.value {
            let attachments = if m.has_attachments {
                match self.attachments(&m.id).await {
                    Ok(a) => a,
                    Err(e) => {
                        // Better to skip the message this poll than to parse it
                        // without the video it carries.
                        warn!("Graph: attachments of {} unreadable, retrying next poll: {e:#}", m.id);
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            out.push(self.to_inbound(m, attachments));
        }
        Ok(out)
    }

    async fn mark_processed(&self, mail_id: &str, outcome: MailOutcome) -> Result<()> {
        let folder = match outcome {
            MailOutcome::Processed => {
                // Read first: if the move fails, a read message is already
                // out of the unread filter and is not fetched again.
                self.send_json(
                    Method::PATCH,
                    self.url(&["messages", mail_id])?,
                    serde_json::json!({ "isRead": true }),
                )
                .await?;
                self.cfg.processed_folder.clone()
            }
            // Left unread: that is what makes a human look at it.
            MailOutcome::Failed => self.cfg.failed_folder.clone(),
        };
        let dest = self.folder_id(&folder).await?;
        self.send_json(
            Method::POST,
            self.url(&["messages", mail_id, "move"])?,
            serde_json::json!({ "destinationId": dest }),
        )
        .await?;
        Ok(())
    }

    async fn download_attachment(&self, mail_id: &str, attachment_id: &str, dest: &Path) -> Result<PathBuf> {
        let url = self.url(&["messages", mail_id, "attachments", attachment_id, "$value"])?;
        let mut resp = self.send(|c| c.get(url.clone())).await?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Streamed: a 2 GB rush file must not be held in memory.
        let partial = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&partial).await?;
        while let Some(chunk) = resp.chunk().await.context("attachment download interrupted")? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, dest).await?;
        Ok(dest.to_path_buf())
    }

    async fn reply(&self, mail_id: &str, html: &str, _text: &str) -> Result<()> {
        self.send_json(
            Method::POST,
            self.url(&["messages", mail_id, "reply"])?,
            serde_json::json!({ "message": { "body": { "contentType": "HTML", "content": html } } }),
        )
        .await?;
        Ok(())
    }

    async fn health(&self) -> MailHealth {
        if !self.is_configured() {
            return MailHealth {
                ok: false,
                detail: "Graph not configured".into(),
            };
        }
        let probe = async {
            let mut url = self.url(&["mailFolders", "Inbox"])?;
            url.query_pairs_mut().append_pair("$select", "id,displayName,unreadItemCount");
            let inbox: GFolder = self.get_json(url, false).await?;
            Ok::<_, anyhow::Error>(inbox.unread_item_count.unwrap_or(0))
        };
        match probe.await {
            Ok(unread) => MailHealth {
                ok: true,
                detail: format!("{}: {unread} unread", self.cfg.mailbox),
            },
            Err(e) => MailHealth {
                ok: false,
                detail: e.to_string().chars().take(200).collect(),
            },
        }
    }
}
