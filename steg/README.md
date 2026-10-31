# Steg

Steg is a [Model Context Protocol](https://modelcontextprotocol.io) server
built into WezTerm. It lets an AI agent drive the terminal the human is
looking at: create tabs, type into panes, press keys, wait for a program to
settle, and read what is on screen.

The original use case: a Grok agent orchestrating several Claude Code
sessions, each in its own tab, in a window you can watch and step into at
any time.

Steg speaks to the WezTerm multiplexer over the same socket `wezterm cli`
uses, so it works with an unmodified `wezterm-gui` of the same version and
adds nothing to the rendering path. It runs on Windows, macOS and Linux.

## Build

```sh
cargo build --release -p steg
# binary: target/release/steg (steg.exe on Windows)
```

Steg must be the same version as the WezTerm it talks to (the mux protocol
is versioned), so build it from the same checkout as your `wezterm-gui`.

## Connect an agent

Start WezTerm first. Then register Steg as a stdio MCP server in whatever is
hosting the agent. For a client that takes the usual JSON shape:

```json
{
  "mcpServers": {
    "steg": {
      "command": "/path/to/steg"
    }
  }
}
```

For Claude Code: `claude mcp add steg -- /path/to/steg`.

Flags: `--class NAME` if you start WezTerm with `--class`, `--prefer-mux` to
target a background `wezterm-mux-server` instead of the GUI, `--auto-start`
to allow Steg to start a mux server when nothing is running (off by default,
so an agent never ends up working in a terminal nobody can see).
Logging goes to stderr; `WEZTERM_LOG=steg=debug` turns it up.

## What the agent gets

Tools:

| tool | what it does |
|---|---|
| `list_panes` | `{panes: [...]}`: every pane with window, tab, title, size, cwd, active flag |
| `spawn_tab` | new tab (or window) running a shell or a command; returns pane_id |
| `split_pane` | split a pane left/right/top/bottom; returns pane_id |
| `send_text` | type text; `enter: true` presses Enter; `paste: true` pastes |
| `send_key` | press a key with modifiers: Escape, Enter, Shift+Tab, Ctrl-C … |
| `get_text` | read the visible screen, or a line range into the scrollback |
| `wait_for_idle` | block until the screen stops changing, then return it |
| `activate_pane` / `activate_tab` | bring something to the front |
| `set_tab_title` | rename a tab |
| `kill_pane` | close a pane |

Resources: `wezterm://pane/{pane_id}/screen` and
`wezterm://pane/{pane_id}/scrollback`, plus one listed resource per live pane.

The server's `instructions` describe the intended flow
(`spawn_tab` → `send_text` → `wait_for_idle` → `get_text`), so a capable
agent needs no further prompting.

Every failure the agent can act on (no terminal running, unknown pane, bad
key name, invalid side) comes back as a tool result with `isError: true`
and a plain-language message, so it reaches the model as text in any
client. Only resource reads use protocol-level errors, because MCP gives
them no other channel.

## Tests

```sh
cargo build -p wezterm-mux-server   # optional: enables the end-to-end tests
cargo test -p steg
```

Unit tests cover key and URI parsing. `tests/mcp_surface.rs` drives the
server through a real MCP client in-process, with no terminal present, and
pins down the handshake, tool schemas, resources and error texts.
`tests/e2e_mux.rs` runs the built `steg` binary against a headless
`wezterm-mux-server` over the real mux protocol — the same path as the GUI —
and exercises every tool against a live bash. See `QA.md` for the manual
checklist against a real window.

## Layout

```
steg/src/main.rs        CLI flags, bootstrap, tokio runtime, stdio transport
steg/src/lib.rs         the library the binary and the tests share
steg/src/server.rs      MCP tools, resources, instructions (rmcp)
steg/src/backend.rs     the wezterm codec: everything `wezterm cli` can do, as async fns
steg/src/keys.rs        "CTRL|c", "Escape", "F5" → key events
steg/tests/mcp_surface.rs   MCP client ↔ server in-process, no terminal
steg/tests/e2e_mux.rs       the binary against a headless wezterm-mux-server
steg/QA.md              automated layers + manual checklist for the GUI
```

`backend.rs` mirrors `wezterm/src/cli/*.rs` on purpose: when the CLI gains a
capability, it can be surfaced here in a few lines.

## Roadmap

- **Push instead of poll.** Subscribe to pane output and emit
  `notifications/resources/updated` for `wezterm://pane/{id}/screen`, and an
  "output settled" notification, so agents stop polling `wait_for_idle`.
  For shells, use OSC 133 semantic zones so "command finished" is exact.
  For Claude Code tabs, let its `Stop` hook ping Steg.
- **Policy gate in Lua.** A `steg_policy` callback in `wezterm.lua` that
  can allow/deny/confirm each tool call (e.g. only panes Steg created, no
  `kill_pane` on the human's panes).
- **Streamable HTTP transport** served from the GUI process, so remote agents
  can connect without a stdio launcher.
- **`wezterm mcp` subcommand** once the surface settles, so it ships inside
  the main binary.
