//! The connection to the WezTerm multiplexer and the operations Steg
//! performs on it.
//!
//! This is the only module that speaks the wezterm codec.  It mirrors what
//! `wezterm cli` does (see `wezterm/src/cli/`), so anything that works from
//! the CLI can be surfaced here, and the server side needs no changes.
//! Everything in `server.rs` is MCP-shaped and talks to this module.

use anyhow::{anyhow, Context};
use codec::{
    GetLines, GetPaneRenderableDimensions, InputSerial, KillPane, ListPanesResponse, SendKeyDown,
    SendPaste, SetFocusedPane, SpawnV2, TabTitleChanged, WriteToPane,
};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::connui::ConnectionUI;
use mux::pane::PaneId;
use mux::tab::{SplitDirection, SplitRequest, SplitSize, TabId};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use schemars::JsonSchema;
use serde::Serialize;
use std::ffi::OsString;
use std::time::{Duration, Instant};
use termwiz::input::KeyEvent;
use termwiz_funcs::lines_to_escapes;
use tokio::sync::Mutex;
use wezterm_client::client::Client;
use wezterm_term::StableRowIndex;

/// How to find the wezterm instance to drive.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// The gui window class; must match `wezterm start --class` if used.
    pub class: String,
    /// Prefer a background mux server over the gui.
    pub prefer_mux: bool,
    /// Refuse to start a mux server when nothing is running.
    pub no_auto_start: bool,
}

/// A connection to the multiplexer, established lazily on first use and
/// re-established if the terminal goes away and comes back.
pub struct Backend {
    connect: ConnectOptions,
    config: ConfigHandle,
    client: Mutex<Option<Client>>,
}

/// One pane, flattened out of the mux's window/tab/pane tree.
/// Field names match `wezterm cli list --format json` where they overlap,
/// so anyone who knows the CLI output knows this.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PaneInfo {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    /// The workspace the window belongs to.
    pub workspace: String,
    /// The pane title, usually set by the running program.
    pub title: String,
    pub tab_title: String,
    pub window_title: String,
    /// Current working directory as a URL (`file://host/path`), if known.
    pub cwd: String,
    pub rows: usize,
    pub cols: usize,
    /// Whether this is the active pane of its tab.
    pub is_active: bool,
    pub is_zoomed: bool,
    /// Cursor column, 0-based from the left of the pane.
    pub cursor_x: usize,
    /// Cursor row, 0-based from the top of the visible screen.
    pub cursor_y: usize,
    pub tty_name: Option<String>,
}

/// The identity of a pane created by `spawn_tab` or `split_pane`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Spawned {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: WindowId,
}

/// Request to spawn a new tab (or window).
#[derive(Debug, Default)]
pub struct SpawnTab {
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub new_window: bool,
    pub window_id: Option<WindowId>,
    pub pane_id: Option<PaneId>,
    pub workspace: Option<String>,
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum SplitSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// Request to split an existing pane.
#[derive(Debug)]
pub struct Split {
    pub pane_id: PaneId,
    pub side: SplitSide,
    pub percent: Option<u8>,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub top_level: bool,
}

/// Outcome of `wait_for_idle`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IdleResult {
    /// True if the screen stopped changing for the requested quiet period;
    /// false if the timeout elapsed first.
    pub idle: bool,
    pub waited_ms: u64,
    /// The visible screen at the moment we stopped waiting.
    pub screen: String,
}

fn flatten_panes(panes: ListPanesResponse) -> Vec<PaneInfo> {
    let mut out = vec![];
    for (tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
        let mut cursor = tabroot.into_tree().cursor();
        loop {
            if let Some(entry) = cursor.leaf_mut() {
                let window_title = panes
                    .window_titles
                    .get(&entry.window_id)
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(PaneInfo {
                    window_id: entry.window_id,
                    tab_id: entry.tab_id,
                    pane_id: entry.pane_id,
                    workspace: entry.workspace.clone(),
                    title: entry.title.clone(),
                    tab_title: tab_title.clone(),
                    window_title,
                    cwd: entry
                        .working_dir
                        .as_ref()
                        .map(|url| url.url.as_str().to_string())
                        .unwrap_or_default(),
                    rows: entry.size.rows,
                    cols: entry.size.cols,
                    is_active: entry.is_active_pane,
                    is_zoomed: entry.is_zoomed_pane,
                    cursor_x: entry.cursor_pos.x,
                    cursor_y: entry.cursor_pos.y.saturating_sub(entry.physical_top) as usize,
                    tty_name: entry.tty_name.clone(),
                });
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(_) => break,
            }
        }
    }
    out
}

fn command_builder(argv: Vec<String>) -> Option<CommandBuilder> {
    if argv.is_empty() {
        None
    } else {
        Some(CommandBuilder::from_argv(
            argv.into_iter().map(OsString::from).collect(),
        ))
    }
}

impl Backend {
    pub fn new(connect: ConnectOptions, config: ConfigHandle) -> Self {
        Self {
            connect,
            config,
            client: Mutex::new(None),
        }
    }

    /// Get a live client, connecting (or reconnecting) if needed.
    ///
    /// The returned `Client` is a cheap clone; its futures only await
    /// channels, so they are happy to run on the tokio runtime even though
    /// wezterm itself uses smol internally.
    async fn client(&self) -> anyhow::Result<Client> {
        let mut guard = self.client.lock().await;

        if let Some(client) = guard.as_ref() {
            if client.ping().await.is_ok() {
                return Ok(client.clone());
            }
            log::warn!("lost the connection to wezterm; reconnecting");
            *guard = None;
        }

        let opts = self.connect.clone();
        let client = tokio::task::spawn_blocking(move || {
            let mut ui = ConnectionUI::new_headless();
            Client::new_default_unix_domain(
                true,
                &mut ui,
                opts.no_auto_start,
                opts.prefer_mux,
                &opts.class,
            )
        })
        .await
        .context("connect task panicked")?
        .with_context(|| {
            if self.connect.no_auto_start {
                "could not connect to a running WezTerm. Start WezTerm (the GUI) first: \
                 steg drives the terminal you can see, and will not start a hidden \
                 mux server unless run with --auto-start"
                    .to_string()
            } else {
                "could not connect to or start a wezterm mux server".to_string()
            }
        })?;

        *guard = Some(client.clone());
        Ok(client)
    }

    pub async fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>> {
        let client = self.client().await?;
        let panes = client.list_panes().await?;
        Ok(flatten_panes(panes))
    }

    async fn find_pane(&self, pane_id: PaneId) -> anyhow::Result<PaneInfo> {
        self.list_panes()
            .await?
            .into_iter()
            .find(|p| p.pane_id == pane_id)
            .ok_or_else(|| {
                anyhow!("pane {pane_id} does not exist; call list_panes to see what does")
            })
    }

    pub async fn tab_for_pane(&self, pane_id: PaneId) -> anyhow::Result<TabId> {
        Ok(self.find_pane(pane_id).await?.tab_id)
    }

    /// Pick the window a new tab should go into when the caller didn't say:
    /// the window holding the pane they named, else the most recently
    /// focused pane's window, else the first window we can find.
    async fn default_window(&self, client: &Client, pane_id: Option<PaneId>) -> Option<WindowId> {
        let panes = match client.list_panes().await {
            Ok(p) => flatten_panes(p),
            Err(_) => return None,
        };

        let pane_id = match pane_id {
            Some(id) => Some(id),
            None => client.resolve_pane_id(None).await.ok(),
        };

        if let Some(id) = pane_id {
            if let Some(p) = panes.iter().find(|p| p.pane_id == id) {
                return Some(p.window_id);
            }
        }
        panes.first().map(|p| p.window_id)
    }

    pub async fn spawn_tab(&self, req: SpawnTab) -> anyhow::Result<Spawned> {
        let client = self.client().await?;

        let window_id = if req.new_window {
            None
        } else {
            match req.window_id {
                Some(w) => Some(w),
                None => self.default_window(&client, req.pane_id).await,
            }
        };

        let workspace = req
            .workspace
            .as_deref()
            .unwrap_or(
                self.config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let size = self.config.initial_size(0, None);

        let spawned = client
            .spawn_v2(SpawnV2 {
                domain: req
                    .domain
                    .map_or(SpawnTabDomain::DefaultDomain, SpawnTabDomain::DomainName),
                window_id,
                command: command_builder(req.command),
                command_dir: req.cwd,
                size,
                workspace,
            })
            .await
            .context("spawning tab")?;

        Ok(Spawned {
            pane_id: spawned.pane_id,
            tab_id: spawned.tab_id,
            window_id: spawned.window_id,
        })
    }

    pub async fn split_pane(&self, req: Split) -> anyhow::Result<Spawned> {
        let client = self.client().await?;

        let (direction, target_is_second) = match req.side {
            SplitSide::Left => (SplitDirection::Horizontal, false),
            SplitSide::Right => (SplitDirection::Horizontal, true),
            SplitSide::Top => (SplitDirection::Vertical, false),
            SplitSide::Bottom => (SplitDirection::Vertical, true),
        };

        let spawned = client
            .split_pane(codec::SplitPane {
                pane_id: req.pane_id,
                split_request: SplitRequest {
                    direction,
                    target_is_second,
                    size: SplitSize::Percent(req.percent.unwrap_or(50).clamp(1, 99)),
                    top_level: req.top_level,
                },
                domain: SpawnTabDomain::CurrentPaneDomain,
                command: command_builder(req.command),
                command_dir: req.cwd,
                move_pane_id: None,
            })
            .await
            .context("splitting pane")?;

        Ok(Spawned {
            pane_id: spawned.pane_id,
            tab_id: spawned.tab_id,
            window_id: spawned.window_id,
        })
    }

    /// Type text into a pane.  With `paste=false` the bytes go straight to
    /// the program as if typed; with `paste=true` they are delivered as a
    /// (bracketed, if the program enabled it) paste.
    pub async fn send_text(&self, pane_id: PaneId, text: &str, paste: bool) -> anyhow::Result<()> {
        let client = self.client().await?;
        if paste {
            client
                .send_paste(SendPaste {
                    pane_id,
                    data: text.to_string(),
                })
                .await?;
        } else {
            client
                .write_to_pane(WriteToPane {
                    pane_id,
                    data: text.as_bytes().to_vec(),
                })
                .await?;
        }
        Ok(())
    }

    pub async fn send_key(&self, pane_id: PaneId, event: KeyEvent) -> anyhow::Result<()> {
        let client = self.client().await?;
        client
            .key_down(SendKeyDown {
                pane_id,
                event,
                input_serial: InputSerial::now(),
            })
            .await?;
        Ok(())
    }

    /// Read lines from a pane.  Line numbers follow `wezterm cli get-text`:
    /// 0 is the top of the visible screen, negative numbers reach back into
    /// the scrollback, `None` means top/bottom of the visible screen.
    pub async fn get_text(
        &self,
        pane_id: PaneId,
        start_line: Option<i32>,
        end_line: Option<i32>,
        escapes: bool,
    ) -> anyhow::Result<String> {
        let client = self.client().await?;
        let info = client
            .get_dimensions(GetPaneRenderableDimensions { pane_id })
            .await
            .with_context(|| format!("reading pane {pane_id}; does it exist?"))?;
        let dims = &info.dimensions;

        let resolve = |n: Option<i32>, default: StableRowIndex| -> StableRowIndex {
            match n {
                None => default,
                Some(n) if n >= 0 => dims.physical_top + n as StableRowIndex,
                Some(n) => {
                    let line = dims.physical_top as isize + n as isize;
                    line.max(dims.scrollback_top as isize) as StableRowIndex
                }
            }
        };

        let start = resolve(start_line, dims.physical_top);
        let end = resolve(
            end_line,
            dims.physical_top + dims.viewport_rows as StableRowIndex,
        );

        self.read_lines(&client, pane_id, start, end, escapes).await
    }

    /// Everything the pane remembers: scrollback plus the visible screen.
    pub async fn get_scrollback(&self, pane_id: PaneId) -> anyhow::Result<String> {
        let client = self.client().await?;
        let info = client
            .get_dimensions(GetPaneRenderableDimensions { pane_id })
            .await
            .with_context(|| format!("reading pane {pane_id}; does it exist?"))?;
        let dims = &info.dimensions;
        let end = dims.physical_top + dims.viewport_rows as StableRowIndex;
        self.read_lines(&client, pane_id, dims.scrollback_top, end, false)
            .await
    }

    async fn read_lines(
        &self,
        client: &Client,
        pane_id: PaneId,
        start: StableRowIndex,
        end: StableRowIndex,
        escapes: bool,
    ) -> anyhow::Result<String> {
        let lines = client
            .get_lines(GetLines {
                pane_id,
                lines: vec![start..end + 1],
            })
            .await?;

        let lines: Vec<_> = lines
            .lines
            .extract_data()
            .0
            .into_iter()
            .map(|(_idx, line)| line)
            .collect();

        if escapes {
            lines_to_escapes(lines)
        } else {
            let mut text: Vec<String> = lines
                .iter()
                .map(|line| line.as_str().trim_end().to_string())
                .collect();
            // Drop the trailing blank rows of the viewport; they are
            // noise for a reader and cost tokens for an agent.
            while text.last().map(|l| l.is_empty()).unwrap_or(false) {
                text.pop();
            }
            Ok(text.join("\n"))
        }
    }

    pub async fn kill_pane(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let client = self.client().await?;
        client.kill_pane(KillPane { pane_id }).await?;
        Ok(())
    }

    pub async fn activate_pane(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let client = self.client().await?;
        client
            .set_focused_pane_id(SetFocusedPane { pane_id })
            .await?;
        Ok(())
    }

    pub async fn activate_tab(&self, tab_id: TabId) -> anyhow::Result<()> {
        let panes = self.list_panes().await?;
        let target = panes
            .iter()
            .find(|p| p.tab_id == tab_id && p.is_active)
            .or_else(|| panes.iter().find(|p| p.tab_id == tab_id))
            .ok_or_else(|| {
                anyhow!("tab {tab_id} does not exist; call list_panes to see what does")
            })?;
        self.activate_pane(target.pane_id).await
    }

    pub async fn set_tab_title(&self, tab_id: TabId, title: String) -> anyhow::Result<()> {
        let client = self.client().await?;
        client
            .set_tab_title(TabTitleChanged { tab_id, title })
            .await?;
        Ok(())
    }

    /// Poll the visible screen until it stops changing for `quiet`, or
    /// `timeout` passes.  A TUI such as Claude Code animates while it is
    /// working and holds still when it is waiting for input, which makes
    /// "the screen settled" a good proxy for "it's your turn".
    pub async fn wait_for_idle(
        &self,
        pane_id: PaneId,
        quiet: Duration,
        timeout: Duration,
        poll: Duration,
    ) -> anyhow::Result<IdleResult> {
        let started = Instant::now();
        let mut last = self.get_text(pane_id, None, None, false).await?;
        let mut last_change = Instant::now();

        loop {
            if last_change.elapsed() >= quiet {
                return Ok(IdleResult {
                    idle: true,
                    waited_ms: started.elapsed().as_millis() as u64,
                    screen: last,
                });
            }
            if started.elapsed() >= timeout {
                return Ok(IdleResult {
                    idle: false,
                    waited_ms: started.elapsed().as_millis() as u64,
                    screen: last,
                });
            }

            tokio::time::sleep(poll).await;

            let now = self.get_text(pane_id, None, None, false).await?;
            if now != last {
                last = now;
                last_change = Instant::now();
            }
        }
    }
}
