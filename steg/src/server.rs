//! The MCP surface of Steg: tools, resources and the instructions the
//! server hands to an agent on connect.
//!
//! Nothing here knows about the wezterm codec; it translates MCP calls into
//! `Backend` operations and formats the answers.

use crate::backend::{Backend, IdleResult, PaneInfo, SpawnTab, Spawned, Split, SplitSide};
use crate::keys::parse_key_event;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::window::WindowId;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const INSTRUCTIONS: &str = "\
Steg exposes a live WezTerm terminal to you. Everything you do happens in the \
window the human is looking at: tabs you spawn appear in their terminal, and \
text you send is typed into a real pane.

Typical flow:
1. list_panes — see what windows, tabs and panes exist.
2. spawn_tab — make a tab, optionally running a command (e.g. [\"claude\"]). \
   You get back a pane_id; use it for everything else.
3. send_text with enter=true — type a line and press Enter. Use send_key for \
   control keys (Escape, Enter, Tab with SHIFT, Ctrl-C as key=\"c\" mods=\"CTRL\").
4. wait_for_idle — block until the pane's screen stops changing, which for an \
   interactive program means it is waiting for you. Returns the screen.
5. get_text — read the visible screen (or scrollback with negative start_line). \
   The same content is available as the resource wezterm://pane/{pane_id}/screen.

Prefer working in tabs you created and leave the human's own panes alone \
unless asked. Pane ids are stable for the life of the pane.";

#[derive(Clone)]
pub struct Steg {
    backend: Arc<Backend>,
    tool_router: ToolRouter<Self>,
}

fn mcp_err(err: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{err:#}"), None)
}

// ---------------------------------------------------------------------------
// Tool parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnTabParams {
    /// Program to run instead of the default shell, as argv
    /// (e.g. ["claude"] or ["bash", "-l"]). Omit for a plain shell.
    #[serde(default)]
    pub command: Vec<String>,
    /// Working directory for the new pane. Absolute path.
    pub cwd: Option<String>,
    /// Open a new window instead of a tab in an existing one.
    #[serde(default)]
    pub new_window: bool,
    /// Window to add the tab to. Defaults to the window of `pane_id`, else
    /// the most recently focused window.
    pub window_id: Option<WindowId>,
    /// A pane whose window should receive the new tab.
    pub pane_id: Option<PaneId>,
    /// Workspace name for a new window (requires new_window).
    pub workspace: Option<String>,
    /// Mux domain name to spawn into (advanced; defaults to the default domain).
    pub domain: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SplitPaneParams {
    /// The pane to split.
    pub pane_id: PaneId,
    /// Where the new pane goes: "left", "right", "top" or "bottom" (default).
    pub side: Option<String>,
    /// Size of the new pane as a percentage of the available space (default 50).
    pub percent: Option<u8>,
    /// Program to run in the new pane, as argv. Omit for a shell.
    #[serde(default)]
    pub command: Vec<String>,
    /// Working directory for the new pane. Absolute path.
    pub cwd: Option<String>,
    /// Split the whole tab rather than just this pane.
    #[serde(default)]
    pub top_level: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendTextParams {
    pub pane_id: PaneId,
    /// The text to type. Sent exactly as given; no newline is added unless
    /// `enter` is true.
    pub text: String,
    /// Press Enter after the text (sends a carriage return).
    #[serde(default)]
    pub enter: bool,
    /// Deliver as a paste (bracketed if the program supports it) rather
    /// than keystrokes. Use for multi-line text going into an editor or a
    /// TUI such as Claude Code.
    #[serde(default)]
    pub paste: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendKeyParams {
    pub pane_id: PaneId,
    /// A single character ("c", "/") or a named key: Enter, Escape, Tab,
    /// Backspace, Delete, Insert, Home, End, PageUp, PageDown, Up, Down,
    /// Left, Right, Space, F1-F24.
    pub key: String,
    /// Modifiers, `|`-separated: SHIFT, CTRL, ALT, SUPER. E.g. "CTRL" or "CTRL|SHIFT".
    pub mods: Option<String>,
    /// How many times to press it (default 1).
    pub repeat: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTextParams {
    pub pane_id: PaneId,
    /// First line to read. 0 is the top of the visible screen; negative
    /// numbers reach back into the scrollback (-50 = fifty lines above the
    /// screen). Default: top of the visible screen.
    pub start_line: Option<i32>,
    /// Last line to read, same numbering. Default: bottom of the visible screen.
    pub end_line: Option<i32>,
    /// Include the escape sequences that colour and style the text.
    #[serde(default)]
    pub escapes: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForIdleParams {
    pub pane_id: PaneId,
    /// How long the screen must stay unchanged to count as idle (default 1500).
    pub quiet_ms: Option<u64>,
    /// Give up after this long (default 120000).
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PaneIdParams {
    pub pane_id: PaneId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TabIdParams {
    pub tab_id: TabId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetTabTitleParams {
    /// The tab to rename. Give either this or pane_id.
    pub tab_id: Option<TabId>,
    /// A pane whose tab should be renamed.
    pub pane_id: Option<PaneId>,
    pub title: String,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl Steg {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend: Arc::new(backend),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List every pane in the running WezTerm, with its window, tab, title, size, cwd and whether it is the active pane of its tab."
    )]
    async fn list_panes(&self) -> Result<Json<Vec<PaneInfo>>, McpError> {
        self.backend.list_panes().await.map(Json).map_err(mcp_err)
    }

    #[tool(
        description = "Create a new tab (or window) running a shell or the given command. Returns the new pane_id, tab_id and window_id. The tab appears in the human's WezTerm window."
    )]
    async fn spawn_tab(
        &self,
        Parameters(p): Parameters<SpawnTabParams>,
    ) -> Result<Json<Spawned>, McpError> {
        self.backend
            .spawn_tab(SpawnTab {
                cwd: p.cwd,
                command: p.command,
                new_window: p.new_window,
                window_id: p.window_id,
                pane_id: p.pane_id,
                workspace: p.workspace,
                domain: p.domain,
            })
            .await
            .map(Json)
            .map_err(mcp_err)
    }

    #[tool(
        description = "Split an existing pane and run a shell or command in the new half. Returns the new pane_id."
    )]
    async fn split_pane(
        &self,
        Parameters(p): Parameters<SplitPaneParams>,
    ) -> Result<Json<Spawned>, McpError> {
        let side = match p.side.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("bottom") | Some("down") => SplitSide::Bottom,
            Some("top") | Some("up") => SplitSide::Top,
            Some("left") => SplitSide::Left,
            Some("right") => SplitSide::Right,
            Some(other) => {
                return Err(McpError::invalid_params(
                    format!("side must be left, right, top or bottom, not {other:?}"),
                    None,
                ))
            }
        };
        self.backend
            .split_pane(Split {
                pane_id: p.pane_id,
                side,
                percent: p.percent,
                cwd: p.cwd,
                command: p.command,
                top_level: p.top_level,
            })
            .await
            .map(Json)
            .map_err(mcp_err)
    }

    #[tool(
        description = "Type text into a pane as keystrokes (or as a paste). Set enter=true to press Enter afterwards. Nothing is added to the text otherwise."
    )]
    async fn send_text(
        &self,
        Parameters(p): Parameters<SendTextParams>,
    ) -> Result<String, McpError> {
        self.backend
            .send_text(p.pane_id, &p.text, p.paste)
            .await
            .map_err(mcp_err)?;
        if p.enter {
            self.backend
                .send_text(p.pane_id, "\r", false)
                .await
                .map_err(mcp_err)?;
        }
        Ok(format!(
            "sent {} bytes to pane {}{}",
            p.text.len(),
            p.pane_id,
            if p.enter { " and pressed Enter" } else { "" }
        ))
    }

    #[tool(
        description = "Press a key in a pane, with optional modifiers. For control keys and chords: Escape, Enter, Up/Down, Shift+Tab (key=\"Tab\", mods=\"SHIFT\"), Ctrl-C (key=\"c\", mods=\"CTRL\")."
    )]
    async fn send_key(&self, Parameters(p): Parameters<SendKeyParams>) -> Result<String, McpError> {
        let event = parse_key_event(&p.key, p.mods.as_deref())
            .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
        let repeat = p.repeat.unwrap_or(1).clamp(1, 100);
        for _ in 0..repeat {
            self.backend
                .send_key(p.pane_id, event.clone())
                .await
                .map_err(mcp_err)?;
        }
        Ok(format!(
            "pressed {}{} x{} in pane {}",
            p.mods
                .as_deref()
                .map(|m| format!("{m}+"))
                .unwrap_or_default(),
            p.key,
            repeat,
            p.pane_id
        ))
    }

    #[tool(
        description = "Read the text of a pane: the visible screen by default, or a line range reaching into the scrollback with negative start_line. Trailing blank lines are dropped."
    )]
    async fn get_text(&self, Parameters(p): Parameters<GetTextParams>) -> Result<String, McpError> {
        self.backend
            .get_text(p.pane_id, p.start_line, p.end_line, p.escapes)
            .await
            .map_err(mcp_err)
    }

    #[tool(
        description = "Wait until a pane's screen stops changing (the program is idle / waiting for input) or a timeout passes. Returns idle=true/false plus the final screen text. Use after sending a command instead of polling get_text."
    )]
    async fn wait_for_idle(
        &self,
        Parameters(p): Parameters<WaitForIdleParams>,
    ) -> Result<Json<IdleResult>, McpError> {
        let quiet = Duration::from_millis(p.quiet_ms.unwrap_or(1500).clamp(100, 60_000));
        let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(120_000).clamp(100, 3_600_000));
        let poll = Duration::from_millis(250);
        self.backend
            .wait_for_idle(p.pane_id, quiet, timeout, poll)
            .await
            .map(Json)
            .map_err(mcp_err)
    }

    #[tool(description = "Focus a pane, bringing its tab to the front in the human's window.")]
    async fn activate_pane(
        &self,
        Parameters(p): Parameters<PaneIdParams>,
    ) -> Result<String, McpError> {
        self.backend
            .activate_pane(p.pane_id)
            .await
            .map_err(mcp_err)?;
        Ok(format!("pane {} is now focused", p.pane_id))
    }

    #[tool(description = "Bring a tab to the front by tab_id.")]
    async fn activate_tab(
        &self,
        Parameters(p): Parameters<TabIdParams>,
    ) -> Result<String, McpError> {
        self.backend.activate_tab(p.tab_id).await.map_err(mcp_err)?;
        Ok(format!("tab {} is now active", p.tab_id))
    }

    #[tool(description = "Rename a tab, identified by tab_id or by any pane_id inside it.")]
    async fn set_tab_title(
        &self,
        Parameters(p): Parameters<SetTabTitleParams>,
    ) -> Result<String, McpError> {
        let tab_id = match (p.tab_id, p.pane_id) {
            (Some(tab_id), _) => tab_id,
            (None, Some(pane_id)) => self.backend.tab_for_pane(pane_id).await.map_err(mcp_err)?,
            (None, None) => {
                return Err(McpError::invalid_params(
                    "give either tab_id or pane_id",
                    None,
                ))
            }
        };
        self.backend
            .set_tab_title(tab_id, p.title.clone())
            .await
            .map_err(mcp_err)?;
        Ok(format!("tab {tab_id} renamed to {:?}", p.title))
    }

    #[tool(
        description = "Close a pane, killing whatever runs in it. Closing the last pane of a tab closes the tab."
    )]
    async fn kill_pane(&self, Parameters(p): Parameters<PaneIdParams>) -> Result<String, McpError> {
        self.backend.kill_pane(p.pane_id).await.map_err(mcp_err)?;
        Ok(format!("pane {} closed", p.pane_id))
    }
}

// ---------------------------------------------------------------------------
// Server handler: info + resources (tools are wired in by #[tool_handler])
// ---------------------------------------------------------------------------

const SCREEN_URI_TEMPLATE: &str = "wezterm://pane/{pane_id}/screen";
const SCROLLBACK_URI_TEMPLATE: &str = "wezterm://pane/{pane_id}/scrollback";

fn screen_uri(pane_id: PaneId) -> String {
    format!("wezterm://pane/{pane_id}/screen")
}

/// Parse `wezterm://pane/<id>/<screen|scrollback>`.
fn parse_pane_uri(uri: &str) -> Option<(PaneId, &str)> {
    let rest = uri.strip_prefix("wezterm://pane/")?;
    let (id, what) = rest.split_once('/')?;
    let id: PaneId = id.parse().ok()?;
    Some((id, what))
}

#[tool_handler]
impl ServerHandler for Steg {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("steg", env!("CARGO_PKG_VERSION"))
                .with_title("Steg (WezTerm multiplexer)"),
        )
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let panes = self.backend.list_panes().await.map_err(mcp_err)?;
        let resources = panes
            .into_iter()
            .map(|p| {
                Resource::new(screen_uri(p.pane_id), format!("pane-{}", p.pane_id))
                    .with_title(format!("Pane {}: {}", p.pane_id, p.title))
                    .with_description(format!(
                        "Visible screen of pane {} (tab {:?} in window {}, cwd {})",
                        p.pane_id, p.tab_title, p.window_id, p.cwd
                    ))
                    .with_mime_type("text/plain")
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(SCREEN_URI_TEMPLATE, "pane-screen")
                .with_title("Pane screen")
                .with_description("The visible screen of a pane, as plain text")
                .with_mime_type("text/plain"),
            ResourceTemplate::new(SCROLLBACK_URI_TEMPLATE, "pane-scrollback")
                .with_title("Pane scrollback")
                .with_description("Everything a pane remembers: scrollback plus the visible screen")
                .with_mime_type("text/plain"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri.clone();
        let (pane_id, what) = parse_pane_uri(&uri).ok_or_else(|| {
            McpError::resource_not_found(
                format!("unknown resource {uri}; expected {SCREEN_URI_TEMPLATE} or {SCROLLBACK_URI_TEMPLATE}"),
                None,
            )
        })?;

        let text = match what {
            "screen" => self.backend.get_text(pane_id, None, None, false).await,
            "scrollback" => self.backend.get_scrollback(pane_id).await,
            _ => {
                return Err(McpError::resource_not_found(
                    format!("unknown resource {uri}"),
                    None,
                ))
            }
        }
        .map_err(mcp_err)?;

        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri)]).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_uris() {
        assert_eq!(
            parse_pane_uri("wezterm://pane/7/screen"),
            Some((7, "screen"))
        );
        assert_eq!(
            parse_pane_uri("wezterm://pane/12/scrollback"),
            Some((12, "scrollback"))
        );
        assert_eq!(parse_pane_uri("wezterm://tab/1/screen"), None);
        assert_eq!(parse_pane_uri("wezterm://pane/x/screen"), None);
        assert_eq!(screen_uri(3), "wezterm://pane/3/screen");
    }
}
