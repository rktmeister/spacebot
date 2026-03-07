//! Browser automation tool for workers.
//!
//! Provides navigation, element interaction, screenshots, and page observation
//! via headless Chrome using chromiumoxide. Uses an accessibility-tree based
//! ref system for LLM-friendly element addressing.

use crate::config::BrowserConfig;
use chromiumoxide::browser::{Browser, BrowserConfig as ChromeConfig};
use chromiumoxide::fetcher::{BrowserFetcher, BrowserFetcherOptions};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide_cdp::cdp::browser_protocol::accessibility::{
    EnableParams as AxEnableParams, GetFullAxTreeParams,
};
use chromiumoxide_cdp::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType,
};
use chromiumoxide_cdp::cdp::browser_protocol::page::CaptureScreenshotFormat;
use futures::StreamExt as _;
use reqwest::Url;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Validate that a URL is safe for the browser to navigate to.
/// Blocks private/loopback IPs, link-local addresses, and cloud metadata endpoints
/// to prevent server-side request forgery.
fn validate_url(url: &str) -> Result<(), BrowserError> {
    let parsed = Url::parse(url)
        .map_err(|error| BrowserError::new(format!("invalid URL '{url}': {error}")))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(BrowserError::new(format!(
                "scheme '{other}' is not allowed — only http and https are permitted"
            )));
        }
    }

    let Some(host) = parsed.host_str() else {
        return Err(BrowserError::new("URL has no host"));
    };

    // Block cloud metadata endpoints regardless of how the IP resolves
    if host == "metadata.google.internal"
        || host == "169.254.169.254"
        || host == "metadata.aws.internal"
    {
        return Err(BrowserError::new(
            "access to cloud metadata endpoints is blocked",
        ));
    }

    // If the host parses as an IP address, check against blocked ranges
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_blocked_ip(ip)
    {
        return Err(BrowserError::new(format!(
            "navigation to private/loopback address {ip} is blocked"
        )));
    }

    // IPv6 addresses in brackets (url crate strips them for host_str)
    if let Some(stripped) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
        && let Ok(ip) = stripped.parse::<IpAddr>()
        && is_blocked_ip(ip)
    {
        return Err(BrowserError::new(format!(
            "navigation to private/loopback address {ip} is blocked"
        )));
    }

    Ok(())
}

/// Returns true if the IP address belongs to a private, loopback, or
/// link-local range that should not be reachable from the browser tool.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()                             // 127.0.0.0/8
            || v4.is_private()                            // 10/8, 172.16/12, 192.168/16
            || v4.is_link_local()                         // 169.254.0.0/16
            || v4.is_broadcast()                          // 255.255.255.255
            || v4.is_unspecified()                        // 0.0.0.0
            || is_v4_cgnat(v4) // 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                             // ::1
            || v6.is_unspecified()                        // ::
            || is_v6_unique_local(v6)                    // fd00::/8 (fc00::/7)
            || is_v6_link_local(v6)                      // fe80::/10
            || is_v4_mapped_blocked(v6)
        }
    }
}

fn is_v4_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0xC0) == 64 // 100.64.0.0/10
}

fn is_v6_unique_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xFE00) == 0xFC00 // fc00::/7
}

fn is_v6_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xFFC0) == 0xFE80 // fe80::/10
}

/// Check if an IPv6 address is a v4-mapped address (::ffff:x.x.x.x)
/// pointing to a blocked IPv4 range.
fn is_v4_mapped_blocked(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        is_blocked_ip(IpAddr::V4(v4))
    } else {
        false
    }
}

/// Opaque handle to shared browser state that persists across worker lifetimes.
///
/// Held by `RuntimeConfig` when `persist_session = true`. All workers for the
/// same agent clone this handle and share a single browser process / tab set.
pub type SharedBrowserHandle = Arc<Mutex<BrowserState>>;

/// Create a new shared browser handle for use in `RuntimeConfig`.
pub fn new_shared_browser_handle() -> SharedBrowserHandle {
    Arc::new(Mutex::new(BrowserState::new()))
}

/// Tool for browser automation (worker-only).
#[derive(Debug, Clone)]
pub struct BrowserTool {
    state: Arc<Mutex<BrowserState>>,
    config: BrowserConfig,
    screenshot_dir: PathBuf,
}

/// Internal browser state managed across tool invocations.
///
/// When `persist_session` is enabled this struct lives in `RuntimeConfig` (via
/// `SharedBrowserHandle`) and is shared across worker lifetimes. Otherwise each
/// `BrowserTool` owns its own instance.
pub struct BrowserState {
    browser: Option<Browser>,
    /// Background task driving the CDP WebSocket handler.
    _handler_task: Option<JoinHandle<()>>,
    /// Tracked pages by target ID.
    pages: HashMap<String, chromiumoxide::Page>,
    /// Currently active page target ID.
    active_target: Option<String>,
    /// Element ref map from the last snapshot, keyed by ref like "e1".
    element_refs: HashMap<String, ElementRef>,
    /// Counter for generating element refs.
    next_ref: usize,
    /// Chrome's user data directory. For ephemeral sessions this is a random
    /// temp dir cleaned up on drop. For persistent sessions this is a stable
    /// path under the instance dir that survives agent restarts.
    user_data_dir: Option<PathBuf>,
    /// When true, `user_data_dir` is a stable path that should NOT be deleted
    /// on drop — it holds cookies, localStorage, and login sessions that must
    /// survive across agent restarts.
    persistent_profile: bool,
}

impl BrowserState {
    fn new() -> Self {
        Self {
            browser: None,
            _handler_task: None,
            pages: HashMap::new(),
            active_target: None,
            element_refs: HashMap::new(),
            next_ref: 0,
            user_data_dir: None,
            persistent_profile: false,
        }
    }
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        // Browser and handler task are dropped automatically —
        // chromiumoxide's Child has kill_on_drop(true).

        // Persistent profiles store cookies, localStorage, and login sessions
        // that must survive across agent restarts — never delete them.
        if self.persistent_profile {
            return;
        }

        if let Some(dir) = self.user_data_dir.take() {
            // Offload sync fs cleanup to a blocking thread so we don't stall
            // the tokio worker that's dropping this state.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(move || {
                    if let Err(error) = std::fs::remove_dir_all(&dir) {
                        tracing::debug!(
                            path = %dir.display(),
                            %error,
                            "failed to clean up browser user data dir"
                        );
                    }
                });
            } else {
                // Dropped outside a tokio runtime (unlikely) — clean up inline.
                if let Err(error) = std::fs::remove_dir_all(&dir) {
                    eprintln!(
                        "failed to clean up browser user data dir {}: {error}",
                        dir.display()
                    );
                }
            }
        }
    }
}

impl std::fmt::Debug for BrowserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserState")
            .field("has_browser", &self.browser.is_some())
            .field("pages", &self.pages.len())
            .field("active_target", &self.active_target)
            .field("element_refs", &self.element_refs.len())
            .field("persistent_profile", &self.persistent_profile)
            .finish()
    }
}

/// Stored info about an element from the accessibility tree snapshot.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ElementRef {
    role: String,
    name: Option<String>,
    description: Option<String>,
    /// The AX node ID from the accessibility tree.
    ax_node_id: String,
    backend_node_id: Option<i64>,
}

impl BrowserTool {
    /// Create a tool with its own isolated browser state (default, non-persistent).
    pub fn new(config: BrowserConfig, screenshot_dir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(BrowserState::new())),
            config,
            screenshot_dir,
        }
    }

    /// Create a tool backed by a shared browser state handle.
    ///
    /// Used when `persist_session = true`. Multiple workers share the same
    /// `SharedBrowserHandle`, so the browser process and tabs survive across
    /// worker lifetimes.
    pub fn new_shared(
        shared_state: SharedBrowserHandle,
        config: BrowserConfig,
        screenshot_dir: PathBuf,
    ) -> Self {
        Self {
            state: shared_state,
            config,
            screenshot_dir,
        }
    }
}

/// Error type for browser tool operations.
#[derive(Debug, thiserror::Error)]
#[error("Browser error: {message}")]
pub struct BrowserError {
    pub message: String,
}

impl BrowserError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// The action to perform.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAction {
    /// Launch the browser. Must be called before any other action.
    Launch,
    /// Navigate the active tab to a URL.
    Navigate,
    /// Open a new tab, optionally at a URL.
    Open,
    /// List all open tabs.
    Tabs,
    /// Focus a tab by its target ID.
    Focus,
    /// Close a tab by its target ID (or the active tab if omitted).
    CloseTab,
    /// Get an accessibility tree snapshot of the active page with element refs.
    Snapshot,
    /// Perform an interaction on an element by ref.
    Act,
    /// Take a screenshot of the active page or a specific element.
    Screenshot,
    /// Evaluate JavaScript in the active page.
    Evaluate,
    /// Get the page HTML content.
    Content,
    /// Shut down the browser.
    Close,
}

/// The kind of interaction to perform via the `act` action.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActKind {
    /// Click an element by ref.
    Click,
    /// Type text into an element by ref.
    Type,
    /// Press a keyboard key (e.g., "Enter", "Tab", "Escape").
    PressKey,
    /// Hover over an element by ref.
    Hover,
    /// Scroll an element into the viewport by ref.
    ScrollIntoView,
    /// Focus an element by ref.
    Focus,
}

/// Arguments for the browser tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserArgs {
    /// The action to perform.
    pub action: BrowserAction,
    /// URL for navigate/open actions.
    pub url: Option<String>,
    /// Target ID for focus/close_tab actions.
    pub target_id: Option<String>,
    /// Element reference (e.g., "e3") for act/screenshot actions.
    pub element_ref: Option<String>,
    /// Kind of interaction for the act action.
    pub act_kind: Option<ActKind>,
    /// Text to type for act:type.
    pub text: Option<String>,
    /// Key to press for act:press_key (e.g., "Enter", "Tab").
    pub key: Option<String>,
    /// Whether to take a full-page screenshot.
    #[serde(default)]
    pub full_page: bool,
    /// JavaScript expression to evaluate.
    pub script: Option<String>,
}

/// Output from the browser tool.
#[derive(Debug, Serialize)]
pub struct BrowserOutput {
    /// Whether the action succeeded.
    pub success: bool,
    /// Human-readable result message.
    pub message: String,
    /// Page title (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Current URL (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Element snapshot data from the accessibility tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elements: Option<Vec<ElementSummary>>,
    /// List of open tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Vec<TabInfo>>,
    /// Path to saved screenshot file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
    /// JavaScript evaluation result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_result: Option<serde_json::Value>,
    /// Page HTML content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl BrowserOutput {
    fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            title: None,
            url: None,
            elements: None,
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
        }
    }

    fn with_page_info(mut self, title: Option<String>, url: Option<String>) -> Self {
        self.title = title;
        self.url = url;
        self
    }
}

/// Summary of an interactive element from the accessibility tree.
#[derive(Debug, Clone, Serialize)]
pub struct ElementSummary {
    /// Short ref like "e1", "e2" for use in subsequent act calls.
    pub ref_id: String,
    /// ARIA role (e.g., "button", "textbox", "link").
    pub role: String,
    /// Accessible name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Accessible description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Current value (for inputs, sliders, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Info about an open browser tab.
#[derive(Debug, Clone, Serialize)]
pub struct TabInfo {
    pub target_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub active: bool,
}

/// Roles that are interactive and worth assigning refs to.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "checkbox",
    "combobox",
    "link",
    "listbox",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "scrollbar",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "textbox",
    "treeitem",
];

/// Max elements to assign refs to in a single snapshot.
const MAX_ELEMENT_REFS: usize = 200;

impl Tool for BrowserTool {
    const NAME: &'static str = "browser";

    type Error = BrowserError;
    type Args = BrowserArgs;
    type Output = BrowserOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/browser").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["launch", "navigate", "open", "tabs", "focus", "close_tab",
                                 "snapshot", "act", "screenshot", "evaluate", "content", "close"],
                        "description": "The browser action to perform"
                    },
                    "url": {
                        "type": "string",
                        "description": "URL for navigate/open actions"
                    },
                    "target_id": {
                        "type": "string",
                        "description": "Tab target ID for focus/close_tab actions"
                    },
                    "element_ref": {
                        "type": "string",
                        "description": "Element reference from snapshot (e.g., \"e3\") for act/screenshot"
                    },
                    "act_kind": {
                        "type": "string",
                        "enum": ["click", "type", "press_key", "hover", "scroll_into_view", "focus"],
                        "description": "Kind of interaction for the act action"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type for act:type"
                    },
                    "key": {
                        "type": "string",
                        "description": "Key to press for act:press_key (e.g., \"Enter\", \"Tab\", \"Escape\")"
                    },
                    "full_page": {
                        "type": "boolean",
                        "default": false,
                        "description": "Take full-page screenshot instead of viewport only"
                    },
                    "script": {
                        "type": "string",
                        "description": "JavaScript expression to evaluate (requires evaluate_enabled in config)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        match args.action {
            BrowserAction::Launch => self.handle_launch().await,
            BrowserAction::Navigate => self.handle_navigate(args.url).await,
            BrowserAction::Open => self.handle_open(args.url).await,
            BrowserAction::Tabs => self.handle_tabs().await,
            BrowserAction::Focus => self.handle_focus(args.target_id).await,
            BrowserAction::CloseTab => self.handle_close_tab(args.target_id).await,
            BrowserAction::Snapshot => self.handle_snapshot().await,
            BrowserAction::Act => {
                self.handle_act(args.act_kind, args.element_ref, args.text, args.key)
                    .await
            }
            BrowserAction::Screenshot => {
                self.handle_screenshot(args.element_ref, args.full_page)
                    .await
            }
            BrowserAction::Evaluate => self.handle_evaluate(args.script).await,
            BrowserAction::Content => self.handle_content().await,
            BrowserAction::Close => self.handle_close().await,
        }
    }
}

impl BrowserTool {
    async fn handle_launch(&self) -> Result<BrowserOutput, BrowserError> {
        // Quick check under the lock — if a browser is already running and
        // we're in persistent mode, reconnect to existing tabs.
        {
            let mut state = self.state.lock().await;
            if state.browser.is_some() {
                if self.config.persist_session {
                    return self.reconnect_existing_tabs(&mut state).await;
                }
                return Ok(BrowserOutput::success("Browser already running"));
            }
        }

        // Resolve the Chrome executable path (may download ~150MB on first use):
        //   1. Explicit config override
        //   2. CHROME / CHROME_PATH env vars
        //   3. chromiumoxide default detection (system PATH + well-known paths)
        //   4. Auto-download via BrowserFetcher (cached in chrome_cache_dir)
        let executable = resolve_chrome_executable(&self.config).await?;

        // Persistent sessions use a stable profile dir under chrome_cache_dir so
        // cookies, localStorage, and login sessions survive across agent restarts.
        // Ephemeral sessions use a random temp dir to avoid singleton lock collisions.
        let (user_data_dir, persistent_profile) = if self.config.persist_session {
            (self.config.chrome_cache_dir.join("profile"), true)
        } else {
            let dir =
                std::env::temp_dir().join(format!("spacebot-browser-{}", uuid::Uuid::new_v4()));
            (dir, false)
        };

        // Chrome writes a SingletonLock file to prevent multiple instances from
        // sharing a profile. When the agent restarts after a crash or kill, the
        // lock file is left behind as a stale artifact. Remove it so Chrome can
        // launch with the persistent profile.
        if persistent_profile {
            let lock_file = user_data_dir.join("SingletonLock");
            if lock_file.exists() {
                tracing::debug!(path = %lock_file.display(), "removing stale Chrome SingletonLock");
                let _ = std::fs::remove_file(&lock_file);
            }
        }

        let mut builder = ChromeConfig::builder()
            .no_sandbox()
            .chrome_executable(&executable)
            .user_data_dir(&user_data_dir);

        if !self.config.headless {
            builder = builder.with_head().window_size(1280, 900);
        }

        let chrome_config = builder.build().map_err(|error| {
            BrowserError::new(format!("failed to build browser config: {error}"))
        })?;

        tracing::info!(
            headless = self.config.headless,
            executable = %executable.display(),
            user_data_dir = %user_data_dir.display(),
            "launching chrome"
        );

        let (browser, mut handler) = Browser::launch(chrome_config)
            .await
            .map_err(|error| BrowserError::new(format!("failed to launch browser: {error}")))?;

        let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

        // Re-acquire the lock only to store state.
        let mut state = self.state.lock().await;

        // Guard against a concurrent launch that won the race.
        if state.browser.is_some() {
            // Another call launched while we were downloading/starting. Clean up
            // the browser we just created and return success.
            drop(browser);
            handler_task.abort();
            // Clean up the temp user data dir — but only for ephemeral sessions.
            // Persistent profiles use a shared stable path that the winner is
            // actively using.
            if !persistent_profile {
                let dir = user_data_dir;
                tokio::spawn(async move {
                    if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
                        tracing::debug!(
                            path = %dir.display(),
                            %error,
                            "failed to clean up browser user data dir (concurrent launch race)"
                        );
                    }
                });
            }

            if self.config.persist_session {
                return self.reconnect_existing_tabs(&mut state).await;
            }
            return Ok(BrowserOutput::success("Browser already running"));
        }

        state.browser = Some(browser);
        state._handler_task = Some(handler_task);
        state.user_data_dir = Some(user_data_dir);
        state.persistent_profile = persistent_profile;

        tracing::info!("browser launched");
        Ok(BrowserOutput::success("Browser launched successfully"))
    }

    /// Discover existing tabs from the browser and rebuild the page map.
    ///
    /// Called when a worker connects to an already-running persistent browser.
    /// Rebuilds `state.pages` from `browser.pages()` so stale entries (tabs
    /// closed externally) are pruned. Validates that `active_target` still
    /// points to a live page.
    ///
    /// Note: holds the mutex across CDP calls. `Browser::pages()` and the
    /// per-tab title/url queries are quick CDP round-trips, and concurrent
    /// browser use during reconnect is rare (workers typically call `launch`
    /// once at the start of their run).
    async fn reconnect_existing_tabs(
        &self,
        state: &mut BrowserState,
    ) -> Result<BrowserOutput, BrowserError> {
        let browser = state
            .browser
            .as_ref()
            .ok_or_else(|| BrowserError::new("browser not launched"))?;

        let pages = browser.pages().await.map_err(|error| {
            BrowserError::new(format!("failed to enumerate existing tabs: {error}"))
        })?;

        // Rebuild the page map from the live browser, pruning stale entries.
        let previous_ids: std::collections::HashSet<String> = state.pages.keys().cloned().collect();
        let mut refreshed_pages = HashMap::with_capacity(pages.len());
        for page in pages {
            let target_id = page_target_id(&page);
            refreshed_pages.insert(target_id, page);
        }
        let discovered = refreshed_pages
            .keys()
            .filter(|id| !previous_ids.contains(*id))
            .count();
        state.pages = refreshed_pages;

        // Ensure active_target points to a valid page.
        let active_valid = state
            .active_target
            .as_ref()
            .is_some_and(|id| state.pages.contains_key(id));
        if !active_valid {
            state.active_target = state.pages.keys().next().cloned();
        }

        let tab_count = state.pages.len();
        let mut tabs = Vec::with_capacity(tab_count);
        for (id, page) in &state.pages {
            let title = page.get_title().await.ok().flatten();
            let url = page.url().await.ok().flatten();
            let is_active = state.active_target.as_deref() == Some(id);
            tabs.push(TabInfo {
                target_id: id.clone(),
                url,
                title,
                active: is_active,
            });
        }

        tracing::info!(tab_count, discovered, "reconnected to persistent browser");

        Ok(BrowserOutput {
            success: true,
            message: format!(
                "Connected to persistent browser ({tab_count} tab{} open, {discovered} newly discovered)",
                if tab_count == 1 { "" } else { "s" }
            ),
            url: None,
            title: None,
            elements: None,
            tabs: Some(tabs),
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }

    async fn handle_navigate(&self, url: Option<String>) -> Result<BrowserOutput, BrowserError> {
        let Some(url) = url else {
            return Err(BrowserError::new("url is required for navigate action"));
        };

        validate_url(&url)?;

        let mut state = self.state.lock().await;
        let page = self.get_or_create_page(&mut state, Some(&url)).await?;

        page.goto(&url)
            .await
            .map_err(|error| BrowserError::new(format!("navigation failed: {error}")))?;

        let title = page.get_title().await.ok().flatten();
        let current_url = page.url().await.ok().flatten();

        // Clear stale element refs on navigation
        state.element_refs.clear();
        state.next_ref = 0;

        Ok(
            BrowserOutput::success(format!("Navigated to {url}"))
                .with_page_info(title, current_url),
        )
    }

    async fn handle_open(&self, url: Option<String>) -> Result<BrowserOutput, BrowserError> {
        let mut state = self.state.lock().await;
        let browser = state
            .browser
            .as_ref()
            .ok_or_else(|| BrowserError::new("browser not launched — call launch first"))?;

        let target_url = url.as_deref().unwrap_or("about:blank");

        if target_url != "about:blank" {
            validate_url(target_url)?;
        }

        let page = browser
            .new_page(target_url)
            .await
            .map_err(|error| BrowserError::new(format!("failed to open tab: {error}")))?;

        let target_id = page_target_id(&page);
        let title = page.get_title().await.ok().flatten();
        let current_url = page.url().await.ok().flatten();

        state.pages.insert(target_id.clone(), page);
        state.active_target = Some(target_id.clone());

        // Clear refs when switching pages
        state.element_refs.clear();
        state.next_ref = 0;

        Ok(BrowserOutput {
            tabs: None,
            elements: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
            success: true,
            message: format!("Opened new tab (target: {target_id})"),
            title,
            url: current_url,
        })
    }

    async fn handle_tabs(&self) -> Result<BrowserOutput, BrowserError> {
        let state = self.state.lock().await;

        let mut tabs = Vec::new();
        for (target_id, page) in &state.pages {
            let title = page.get_title().await.ok().flatten();
            let url = page.url().await.ok().flatten();
            let active = state.active_target.as_ref() == Some(target_id);

            tabs.push(TabInfo {
                target_id: target_id.clone(),
                title,
                url,
                active,
            });
        }

        let count = tabs.len();
        Ok(BrowserOutput {
            success: true,
            message: format!("{count} tab(s) open"),
            title: None,
            url: None,
            elements: None,
            tabs: Some(tabs),
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }

    async fn handle_focus(&self, target_id: Option<String>) -> Result<BrowserOutput, BrowserError> {
        let Some(target_id) = target_id else {
            return Err(BrowserError::new("target_id is required for focus action"));
        };

        let mut state = self.state.lock().await;

        if !state.pages.contains_key(&target_id) {
            return Err(BrowserError::new(format!(
                "no tab with target_id '{target_id}'"
            )));
        }

        state.active_target = Some(target_id.clone());
        state.element_refs.clear();
        state.next_ref = 0;

        Ok(BrowserOutput::success(format!("Focused tab {target_id}")))
    }

    async fn handle_close_tab(
        &self,
        target_id: Option<String>,
    ) -> Result<BrowserOutput, BrowserError> {
        let mut state = self.state.lock().await;

        let id = target_id
            .or_else(|| state.active_target.clone())
            .ok_or_else(|| BrowserError::new("no active tab to close"))?;

        let page = state
            .pages
            .remove(&id)
            .ok_or_else(|| BrowserError::new(format!("no tab with target_id '{id}'")))?;

        page.close()
            .await
            .map_err(|error| BrowserError::new(format!("failed to close tab: {error}")))?;

        if state.active_target.as_ref() == Some(&id) {
            state.active_target = state.pages.keys().next().cloned();
        }

        state.element_refs.clear();
        state.next_ref = 0;

        Ok(BrowserOutput::success(format!("Closed tab {id}")))
    }

    async fn handle_snapshot(&self) -> Result<BrowserOutput, BrowserError> {
        let mut state = self.state.lock().await;
        let page = self.require_active_page(&state)?.clone();

        // Enable accessibility domain if not already enabled
        page.execute(AxEnableParams::default())
            .await
            .map_err(|error| {
                BrowserError::new(format!("failed to enable accessibility: {error}"))
            })?;

        let ax_tree = page
            .execute(GetFullAxTreeParams::default())
            .await
            .map_err(|error| {
                BrowserError::new(format!("failed to get accessibility tree: {error}"))
            })?;

        state.element_refs.clear();
        state.next_ref = 0;

        let mut elements = Vec::new();

        for node in &ax_tree.result.nodes {
            if node.ignored {
                continue;
            }

            let role = extract_ax_value_string(&node.role);
            let Some(role) = role else { continue };

            let role_lower = role.to_lowercase();
            let is_interactive = INTERACTIVE_ROLES.contains(&role_lower.as_str());

            if !is_interactive {
                continue;
            }

            if state.next_ref >= MAX_ELEMENT_REFS {
                break;
            }

            let name = extract_ax_value_string(&node.name);
            let description = extract_ax_value_string(&node.description);
            let value = extract_ax_value_string(&node.value);
            let backend_node_id = node.backend_dom_node_id.as_ref().map(|id| *id.inner());

            let ref_id = format!("e{}", state.next_ref);
            state.next_ref += 1;

            state.element_refs.insert(
                ref_id.clone(),
                ElementRef {
                    role: role.clone(),
                    name: name.clone(),
                    description: description.clone(),
                    ax_node_id: node.node_id.inner().clone(),
                    backend_node_id,
                },
            );

            elements.push(ElementSummary {
                ref_id,
                role,
                name,
                description,
                value,
            });
        }

        let title = page.get_title().await.ok().flatten();
        let url = page.url().await.ok().flatten();
        let count = elements.len();

        Ok(BrowserOutput {
            success: true,
            message: format!("{count} interactive element(s) found"),
            title,
            url,
            elements: Some(elements),
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: None,
        })
    }

    async fn handle_act(
        &self,
        act_kind: Option<ActKind>,
        element_ref: Option<String>,
        text: Option<String>,
        key: Option<String>,
    ) -> Result<BrowserOutput, BrowserError> {
        let Some(act_kind) = act_kind else {
            return Err(BrowserError::new(
                "act_kind is required for act action — must be one of: \
                 click, type, press_key, hover, scroll_into_view, focus. \
                 Example: {\"action\": \"act\", \"act_kind\": \"click\", \"element_ref\": \"e3\"}",
            ));
        };

        let state = self.state.lock().await;
        let page = self.require_active_page(&state)?;

        match act_kind {
            ActKind::Click => {
                let selector_js = self.build_js_selector(&state, element_ref)?;
                let js = format!(
                    r#"(() => {{
                        {selector_js}
                        el.scrollIntoView({{block: 'center'}});
                        el.click();
                        return JSON.stringify({{
                            success: true,
                            tag: el.tagName,
                            text: el.textContent.substring(0, 100).trim()
                        }});
                    }})()"#
                );
                let result = self.run_js_action(page, &js).await?;
                let tag = result
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .unwrap_or("element");
                let text = result.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let display = if text.is_empty() {
                    format!("Clicked {tag}")
                } else {
                    format!("Clicked {tag}: '{}'", truncate_for_display(text, 50))
                };
                Ok(BrowserOutput::success(display))
            }
            ActKind::Type => {
                let Some(text) = text else {
                    return Err(BrowserError::new(
                        "text is required for act_kind: \"type\" — \
                         example: {\"action\": \"act\", \"act_kind\": \"type\", \"element_ref\": \"e5\", \"text\": \"hello\"}",
                    ));
                };
                let selector_js = self.build_js_selector(&state, element_ref)?;
                let text_json = serde_json::to_string(&text).unwrap_or_default();
                let js = format!(
                    r#"(() => {{
                        {selector_js}
                        let txt = {text_json};
                        el.focus();
                        el.value = txt;
                        el.dispatchEvent(new Event('input', {{bubbles: true}}));
                        el.dispatchEvent(new Event('change', {{bubbles: true}}));
                        return JSON.stringify({{success: true}});
                    }})()"#
                );
                self.run_js_action(page, &js).await?;
                Ok(BrowserOutput::success(format!(
                    "Typed '{}' into element",
                    truncate_for_display(&text, 50)
                )))
            }
            ActKind::PressKey => {
                let Some(key) = key else {
                    return Err(BrowserError::new(
                        "key is required for act_kind: \"press_key\" — \
                         example: {\"action\": \"act\", \"act_kind\": \"press_key\", \"key\": \"Enter\"}",
                    ));
                };
                // press_key can work without an element ref (sends to page)
                if element_ref.is_some() {
                    let selector_js = self.build_js_selector(&state, element_ref)?;
                    let key_json = serde_json::to_string(&key).unwrap_or_default();
                    let js = format!(
                        r#"(() => {{
                            {selector_js}
                            el.focus();
                            el.dispatchEvent(new KeyboardEvent('keydown', {{key: {key_json}, bubbles: true}}));
                            el.dispatchEvent(new KeyboardEvent('keyup', {{key: {key_json}, bubbles: true}}));
                            return JSON.stringify({{success: true}});
                        }})()"#
                    );
                    self.run_js_action(page, &js).await?;
                } else {
                    dispatch_key_press(page, &key).await?;
                }
                Ok(BrowserOutput::success(format!("Pressed key '{key}'")))
            }
            ActKind::Hover => {
                let selector_js = self.build_js_selector(&state, element_ref)?;
                let js = format!(
                    r#"(() => {{
                        {selector_js}
                        el.scrollIntoView({{block: 'center'}});
                        el.dispatchEvent(new MouseEvent('mouseover', {{bubbles: true}}));
                        el.dispatchEvent(new MouseEvent('mouseenter', {{bubbles: true}}));
                        return JSON.stringify({{success: true}});
                    }})()"#
                );
                self.run_js_action(page, &js).await?;
                Ok(BrowserOutput::success("Hovered over element"))
            }
            ActKind::ScrollIntoView => {
                let selector_js = self.build_js_selector(&state, element_ref)?;
                let js = format!(
                    r#"(() => {{
                        {selector_js}
                        el.scrollIntoView({{block: 'center', behavior: 'smooth'}});
                        return JSON.stringify({{success: true}});
                    }})()"#
                );
                self.run_js_action(page, &js).await?;
                Ok(BrowserOutput::success("Scrolled element into view"))
            }
            ActKind::Focus => {
                let selector_js = self.build_js_selector(&state, element_ref)?;
                let js = format!(
                    r#"(() => {{
                        {selector_js}
                        el.focus();
                        return JSON.stringify({{success: true}});
                    }})()"#
                );
                self.run_js_action(page, &js).await?;
                Ok(BrowserOutput::success("Focused element"))
            }
        }
    }

    /// Build a JS snippet that resolves an element ref to a DOM element stored in `el`.
    ///
    /// Uses the accessibility tree ref's role and name to build CSS selectors,
    /// with a text-content fallback across all interactive elements. This is
    /// injected into a JS IIFE that must return a JSON result.
    fn build_js_selector(
        &self,
        state: &BrowserState,
        element_ref: Option<String>,
    ) -> Result<String, BrowserError> {
        let Some(ref_id) = element_ref else {
            return Err(BrowserError::new(
                "element_ref is required for this action — run snapshot first, \
                 then use a ref like \"e0\", \"e1\" from the results",
            ));
        };

        let elem_ref = state.element_refs.get(&ref_id).ok_or_else(|| {
            BrowserError::new(format!(
                "unknown element ref '{ref_id}' — run snapshot first to get fresh element refs"
            ))
        })?;

        // Build CSS selectors to try, plus a text-content fallback.
        let selectors = build_selectors_for_ref(elem_ref);
        let selectors_json = serde_json::to_string(&selectors).unwrap_or_default();
        let name_json = serde_json::to_string(&elem_ref.name).unwrap_or("null".to_string());

        // JS that tries each CSS selector, then falls back to text matching
        // across interactive elements. Sets `el` or returns an error.
        Ok(format!(
            r#"let el = null;
            const selectors = {selectors_json};
            for (const sel of selectors) {{
                el = document.querySelector(sel);
                if (el) break;
            }}
            if (!el) {{
                const name = {name_json};
                if (name) {{
                    const candidates = document.querySelectorAll(
                        'a, button, [role="button"], input, select, textarea, [onclick], [tabindex]'
                    );
                    const lower = name.toLowerCase();
                    for (const e of candidates) {{
                        const text = (e.textContent || '').trim().toLowerCase();
                        const label = (e.getAttribute('aria-label') || '').toLowerCase();
                        const title = (e.getAttribute('title') || '').toLowerCase();
                        if (text === lower || label === lower || title === lower) {{ el = e; break; }}
                    }}
                    if (!el) {{
                        for (const e of candidates) {{
                            const text = (e.textContent || '').trim().toLowerCase();
                            if (text.includes(lower)) {{ el = e; break; }}
                        }}
                    }}
                }}
            }}
            if (!el) return JSON.stringify({{
                success: false,
                error: 'Element not found for ref {ref_id}. Run snapshot again to get fresh refs.'
            }});"#
        ))
    }

    /// Execute a JS action and parse the JSON result.
    async fn run_js_action(
        &self,
        page: &chromiumoxide::Page,
        js: &str,
    ) -> Result<serde_json::Value, BrowserError> {
        let result = page
            .evaluate(js)
            .await
            .map_err(|error| BrowserError::new(format!("JS execution failed: {error}")))?;

        let value = result.value().cloned().unwrap_or(serde_json::Value::Null);

        // The JS returns a JSON string — parse it
        let parsed = if let Some(json_str) = value.as_str() {
            serde_json::from_str::<serde_json::Value>(json_str).unwrap_or(value)
        } else {
            value
        };

        // Check for JS-level errors
        let success = parsed
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !success {
            let error = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("action failed");
            return Err(BrowserError::new(error.to_string()));
        }

        Ok(parsed)
    }

    async fn handle_screenshot(
        &self,
        element_ref: Option<String>,
        full_page: bool,
    ) -> Result<BrowserOutput, BrowserError> {
        let state = self.state.lock().await;
        let page = self.require_active_page(&state)?;

        let screenshot_data = if let Some(ref_id) = element_ref {
            // Use JS to find the element and get its bounding rect, then take
            // a clipped page screenshot. This avoids stale CDP node IDs.
            let selector_js = self.build_js_selector(&state, Some(ref_id))?;
            let js = format!(
                r#"(() => {{
                    {selector_js}
                    el.scrollIntoView({{block: 'center'}});
                    const rect = el.getBoundingClientRect();
                    return JSON.stringify({{
                        success: true,
                        x: rect.x + window.scrollX,
                        y: rect.y + window.scrollY,
                        width: rect.width,
                        height: rect.height
                    }});
                }})()"#
            );
            let result = self.run_js_action(page, &js).await?;
            let x = result.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = result.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let width = result
                .get("width")
                .and_then(|v| v.as_f64())
                .unwrap_or(800.0);
            let height = result
                .get("height")
                .and_then(|v| v.as_f64())
                .unwrap_or(600.0);

            use chromiumoxide_cdp::cdp::browser_protocol::page::Viewport;
            let clip = Viewport {
                x,
                y,
                width,
                height,
                scale: 1.0,
            };
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .clip(clip)
                .build();
            page.screenshot(params)
                .await
                .map_err(|error| BrowserError::new(format!("element screenshot failed: {error}")))?
        } else {
            let params = ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(full_page)
                .build();
            page.screenshot(params)
                .await
                .map_err(|error| BrowserError::new(format!("screenshot failed: {error}")))?
        };

        // Save to disk
        let filename = format!(
            "screenshot_{}.png",
            chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f")
        );
        let filepath = self.screenshot_dir.join(&filename);

        tokio::fs::create_dir_all(&self.screenshot_dir)
            .await
            .map_err(|error| {
                BrowserError::new(format!("failed to create screenshot dir: {error}"))
            })?;

        tokio::fs::write(&filepath, &screenshot_data)
            .await
            .map_err(|error| BrowserError::new(format!("failed to save screenshot: {error}")))?;

        let path_str = filepath.to_string_lossy().to_string();
        let size_kb = screenshot_data.len() / 1024;

        tracing::debug!(path = %path_str, size_kb, "screenshot saved");

        Ok(BrowserOutput {
            success: true,
            message: format!("Screenshot saved ({size_kb}KB)"),
            title: None,
            url: None,
            elements: None,
            tabs: None,
            screenshot_path: Some(path_str),
            eval_result: None,
            content: None,
        })
    }

    async fn handle_evaluate(&self, script: Option<String>) -> Result<BrowserOutput, BrowserError> {
        if !self.config.evaluate_enabled {
            return Err(BrowserError::new(
                "JavaScript evaluation is disabled in browser config (set evaluate_enabled = true)",
            ));
        }

        let Some(script) = script else {
            return Err(BrowserError::new("script is required for evaluate action"));
        };

        let state = self.state.lock().await;
        let page = self.require_active_page(&state)?;

        let result = page
            .evaluate(script)
            .await
            .map_err(|error| BrowserError::new(format!("evaluate failed: {error}")))?;

        let value = result.value().cloned();

        Ok(BrowserOutput {
            success: true,
            message: "JavaScript evaluated".to_string(),
            title: None,
            url: None,
            elements: None,
            tabs: None,
            screenshot_path: None,
            eval_result: value,
            content: None,
        })
    }

    async fn handle_content(&self) -> Result<BrowserOutput, BrowserError> {
        let state = self.state.lock().await;
        let page = self.require_active_page(&state)?;

        let html = page
            .content()
            .await
            .map_err(|error| BrowserError::new(format!("failed to get page content: {error}")))?;

        let title = page.get_title().await.ok().flatten();
        let url = page.url().await.ok().flatten();

        // Truncate very large pages for LLM consumption
        let truncated = if html.len() > 100_000 {
            format!(
                "{}... [truncated, {} bytes total]",
                &html[..100_000],
                html.len()
            )
        } else {
            html
        };

        Ok(BrowserOutput {
            success: true,
            message: "Page content retrieved".to_string(),
            title,
            url,
            elements: None,
            tabs: None,
            screenshot_path: None,
            eval_result: None,
            content: Some(truncated),
        })
    }

    async fn handle_close(&self) -> Result<BrowserOutput, BrowserError> {
        use crate::config::ClosePolicy;

        match self.config.close_policy {
            ClosePolicy::Detach => {
                let mut state = self.state.lock().await;
                // Clear per-worker element refs but preserve tabs, browser,
                // handler, and active_target so the next worker picks up
                // exactly where this one left off.
                state.element_refs.clear();
                state.next_ref = 0;
                tracing::info!(policy = "detach", "worker detached from browser");
                Ok(BrowserOutput::success(
                    "Detached from browser (tabs and session preserved)",
                ))
            }
            ClosePolicy::CloseTabs => {
                // Drain pages under the lock, then close them outside it so
                // other workers aren't blocked by CDP round-trips.
                let pages_to_close: Vec<(String, chromiumoxide::Page)> = {
                    let mut state = self.state.lock().await;
                    let pages = state.pages.drain().collect();
                    state.active_target = None;
                    state.element_refs.clear();
                    state.next_ref = 0;
                    pages
                };

                let mut close_errors = Vec::new();
                for (id, page) in pages_to_close {
                    if let Err(error) = page.close().await {
                        close_errors.push(format!("{id}: {error}"));
                    }
                }

                if !close_errors.is_empty() {
                    let message = format!(
                        "failed to close {} tab(s): {}",
                        close_errors.len(),
                        close_errors.join("; ")
                    );
                    tracing::warn!(policy = "close_tabs", %message);
                    return Err(BrowserError::new(message));
                }

                tracing::info!(
                    policy = "close_tabs",
                    "closed all tabs, browser still running"
                );
                Ok(BrowserOutput::success(
                    "All tabs closed (browser still running)",
                ))
            }
            ClosePolicy::CloseBrowser => {
                // Take everything out of state under the lock, then do the
                // actual teardown outside it.
                let (browser, handler_task, user_data_dir, persistent_profile) = {
                    let mut state = self.state.lock().await;
                    let browser = state.browser.take();
                    let handler_task = state._handler_task.take();
                    let user_data_dir = state.user_data_dir.take();
                    let persistent_profile = state.persistent_profile;
                    state.pages.clear();
                    state.active_target = None;
                    state.element_refs.clear();
                    state.next_ref = 0;
                    (browser, handler_task, user_data_dir, persistent_profile)
                };

                if let Some(task) = handler_task {
                    task.abort();
                }

                if let Some(mut browser) = browser
                    && let Err(error) = browser.close().await
                {
                    let message = format!("failed to close browser: {error}");
                    tracing::warn!(policy = "close_browser", %message);
                    return Err(BrowserError::new(message));
                }

                // Clean up the user data dir — but only for ephemeral sessions.
                // Persistent profiles hold cookies and login sessions that must
                // survive browser restarts.
                if !persistent_profile && let Some(dir) = user_data_dir {
                    tokio::spawn(async move {
                        if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
                            tracing::debug!(
                                path = %dir.display(),
                                %error,
                                "failed to clean up browser user data dir"
                            );
                        }
                    });
                }

                tracing::info!(policy = "close_browser", "browser closed");
                Ok(BrowserOutput::success("Browser closed"))
            }
        }
    }

    /// Get the active page, or create a first one if the browser has no pages yet.
    async fn get_or_create_page<'a>(
        &self,
        state: &'a mut BrowserState,
        url: Option<&str>,
    ) -> Result<&'a chromiumoxide::Page, BrowserError> {
        if let Some(target) = state.active_target.as_ref()
            && state.pages.contains_key(target)
        {
            return Ok(&state.pages[target]);
        }

        let browser = state
            .browser
            .as_ref()
            .ok_or_else(|| BrowserError::new("browser not launched — call launch first"))?;

        let target_url = url.unwrap_or("about:blank");
        let page = browser
            .new_page(target_url)
            .await
            .map_err(|error| BrowserError::new(format!("failed to create page: {error}")))?;

        let target_id = page_target_id(&page);
        state.pages.insert(target_id.clone(), page);
        state.active_target = Some(target_id.clone());

        Ok(&state.pages[&target_id])
    }

    /// Get the active page or return an error.
    fn require_active_page<'a>(
        &self,
        state: &'a BrowserState,
    ) -> Result<&'a chromiumoxide::Page, BrowserError> {
        let target = state
            .active_target
            .as_ref()
            .ok_or_else(|| BrowserError::new("no active tab — navigate or open a tab first"))?;

        state
            .pages
            .get(target)
            .ok_or_else(|| BrowserError::new("active tab no longer exists"))
    }
}

/// Dispatch a key press event to the page via CDP Input domain.
async fn dispatch_key_press(page: &chromiumoxide::Page, key: &str) -> Result<(), BrowserError> {
    let key_down = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key(key)
        .build()
        .map_err(|error| BrowserError::new(format!("failed to build key event: {error}")))?;

    page.execute(key_down)
        .await
        .map_err(|error| BrowserError::new(format!("key down failed: {error}")))?;

    let key_up = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(key)
        .build()
        .map_err(|error| BrowserError::new(format!("failed to build key event: {error}")))?;

    page.execute(key_up)
        .await
        .map_err(|error| BrowserError::new(format!("key up failed: {error}")))?;

    Ok(())
}

/// Extract the string value from an AxValue option.
fn extract_ax_value_string(
    ax_value: &Option<chromiumoxide_cdp::cdp::browser_protocol::accessibility::AxValue>,
) -> Option<String> {
    let val = ax_value.as_ref()?;
    val.value
        .as_ref()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

/// Build multiple CSS selectors for an element ref, ordered from most to least specific.
///
/// Native HTML elements (`<button>`, `<a>`, `<input>`) have implicit ARIA roles
/// without explicit `[role]` attributes, so a selector like `[role='button']`
/// won't match a `<button>`. This generates selectors for both the native tag
/// and the explicit role attribute.
fn build_selectors_for_ref(elem_ref: &ElementRef) -> Vec<String> {
    let mut selectors = Vec::with_capacity(4);
    let escaped_name = elem_ref.name.as_ref().map(|n| n.replace('\'', "\\'"));

    // 1. Native tag + aria-label (most specific, most common case)
    if let Some(tag) = role_to_native_tag(&elem_ref.role) {
        if let Some(name) = &escaped_name {
            selectors.push(format!("{tag}[aria-label='{name}']"));
        }
        // 2. Native tag + title attribute
        if let Some(name) = &escaped_name {
            selectors.push(format!("{tag}[title='{name}']"));
        }
        // 3. Native tag alone (broad — only useful if there's one on the page)
        selectors.push(tag.to_string());
    }

    // 4. Explicit role + aria-label (for ARIA widgets)
    if let Some(name) = &escaped_name {
        selectors.push(format!("[role='{}'][aria-label='{name}']", elem_ref.role));
    }

    // 5. Explicit role alone (broadest)
    selectors.push(format!("[role='{}']", elem_ref.role));

    selectors
}

/// Map ARIA roles to native HTML tags that carry the role implicitly.
fn role_to_native_tag(role: &str) -> Option<&'static str> {
    match role {
        "button" => Some("button"),
        "link" => Some("a"),
        "textbox" => Some("input"),
        "searchbox" => Some("input[type='search']"),
        "checkbox" => Some("input[type='checkbox']"),
        "radio" => Some("input[type='radio']"),
        "slider" => Some("input[type='range']"),
        "spinbutton" => Some("input[type='number']"),
        "combobox" => Some("select"),
        "option" => Some("option"),
        "listbox" => Some("select"),
        "menuitem" => Some("menuitem"),
        "tab" => Some("[role='tab']"),
        _ => None,
    }
}

/// Extract target ID string from a Page.
fn page_target_id(page: &chromiumoxide::Page) -> String {
    page.target_id().inner().clone()
}

/// Truncate a string for display, appending "..." if truncated.
fn truncate_for_display(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len])
    }
}

/// Resolve the Chrome/Chromium executable path using a layered detection chain:
///
/// 1. Explicit config override (`executable_path` in TOML)
/// 2. `CHROME` / `CHROME_PATH` environment variables
/// 3. chromiumoxide default detection (system PATH + well-known install paths)
/// 4. Auto-download via `BrowserFetcher` (cached in `chrome_cache_dir`)
async fn resolve_chrome_executable(config: &BrowserConfig) -> Result<PathBuf, BrowserError> {
    // 1. Explicit config
    if let Some(path) = &config.executable_path {
        let path = PathBuf::from(path);
        if path.exists() {
            tracing::debug!(path = %path.display(), "using configured chrome executable");
            return Ok(path);
        }
        tracing::warn!(
            path = %path.display(),
            "configured executable_path does not exist, falling through to detection"
        );
    }

    // 2. Environment variables
    if let Some(path) = detect_chrome_from_env() {
        tracing::debug!(path = %path.display(), "using chrome from environment variable");
        return Ok(path);
    }

    // 3. chromiumoxide default detection (PATH lookup + well-known install paths)
    if let Ok(path) = chromiumoxide::detection::default_executable(Default::default()) {
        tracing::debug!(path = %path.display(), "using system-detected chrome");
        return Ok(path);
    }

    // 4. Auto-download via fetcher
    tracing::info!(
        cache_dir = %config.chrome_cache_dir.display(),
        "no system Chrome found, downloading via fetcher"
    );
    fetch_chrome(&config.chrome_cache_dir).await
}

/// Check `CHROME` and `CHROME_PATH` environment variables for a Chrome binary.
fn detect_chrome_from_env() -> Option<PathBuf> {
    for variable in ["CHROME", "CHROME_PATH"] {
        if let Ok(value) = std::env::var(variable) {
            let path = PathBuf::from(&value);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

/// Download Chromium using chromiumoxide's built-in fetcher.
/// The binary is cached in `cache_dir` and reused on subsequent launches.
async fn fetch_chrome(cache_dir: &Path) -> Result<PathBuf, BrowserError> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|error| {
            BrowserError::new(format!(
                "failed to create chrome cache dir {}: {error}",
                cache_dir.display()
            ))
        })?;

    let options = BrowserFetcherOptions::builder()
        .with_path(cache_dir)
        .build()
        .map_err(|error| {
            BrowserError::new(format!("failed to build browser fetcher options: {error}"))
        })?;

    let fetcher = BrowserFetcher::new(options);
    let info = fetcher
        .fetch()
        .await
        .map_err(|error| BrowserError::new(format!("failed to download chrome: {error}")))?;

    tracing::info!(
        path = %info.executable_path.display(),
        "chrome downloaded and cached"
    );
    Ok(info.executable_path)
}
