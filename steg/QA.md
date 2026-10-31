# Steg QA

What is verified automatically, and what still needs a human in front of a
real WezTerm window.

## Automated

Run from the repo root.

```sh
cargo fmt -p steg --check                      # formatting
cargo clippy -p steg --tests --no-deps         # lints (upstream crates are excluded on purpose)
cargo build -p wezterm-mux-server              # enables the end-to-end tests
cargo test -p steg                             # unit + MCP surface + end-to-end
```

Three layers:

1. **Unit** (`src/keys.rs`, `src/server.rs`): key-name and modifier parsing,
   `wezterm://pane/{id}/…` URI parsing.
2. **MCP surface** (`tests/mcp_surface.rs`): a real MCP client talks to the
   server in-process over a pipe, with no terminal available. Pins down the
   handshake (identity, capabilities, instructions), the full tool list with
   schemas and docs, resource templates, and that every failure is a clean
   tool error the agent can read: "start WezTerm", "unknown key", "side
   must be…". Also checks the connection survives malformed calls.
3. **End-to-end** (`tests/e2e_mux.rs`): the built `steg` binary drives a
   headless `wezterm-mux-server` over the real mux protocol, the same path
   used for `wezterm-gui`. Spawns bash in a new tab, types a command, waits
   for idle, reads the screen and the scrollback (as tools and as
   resources), interrupts a `sleep` with Ctrl-C, renames the tab, splits,
   activates, kills, and checks the listing agrees at every step. Skips
   with a note if `wezterm-mux-server` isn't built.

The e2e test is also an implicit check that nothing writes to stdout except
JSON-RPC: a stray print would corrupt the stream and fail the handshake.

## Manual: against the GUI

Not reproducible in CI without a display, so it's a checklist. Build
`wezterm-gui`, `wezterm` and `steg` from the **same checkout** (the mux
protocol is versioned), start WezTerm, then poke the server. The quickest
driver is the MCP Inspector, which gives you buttons for every tool:

```sh
npx @modelcontextprotocol/inspector target/release/steg
```

or register it with Claude Code and let it do the driving:

```sh
claude mcp add steg -- /path/to/steg        # Windows: C:\path\to\steg.exe
```

Tick these off on each platform, Windows first.

**Connect**
- [ ] `list_panes` returns the panes you can see, with the right titles and cwds.
- [ ] Quit WezTerm, call a tool: error says to start WezTerm (no hidden mux server appears in Task Manager / `ps`).
- [ ] Start WezTerm again, call a tool: works without restarting steg.
- [ ] Started WezTerm with `--class foo`? `steg --class foo` finds it; without the flag it reports no instance.

**Tabs and panes**
- [ ] `spawn_tab` with no command opens a tab running your default shell in the window you're looking at, and focus moves to it.
- [ ] `spawn_tab` with `"new_window": true` opens a second window.
- [ ] `spawn_tab` with `"command": ["claude"]` starts Claude Code in the tab (Windows: also try `["pwsh"]` and `["cmd", "/c", "dir"]`).
- [ ] `spawn_tab` with `"cwd"` set lands the shell in that directory (Windows: a path with backslashes).
- [ ] `split_pane` right/bottom at 40% looks like 40%.
- [ ] `set_tab_title` changes the visible tab title; `list_panes` reports it.
- [ ] `activate_tab` / `activate_pane` move focus visibly.
- [ ] `kill_pane` closes the pane; the last pane in a tab closes the tab.

**Typing and reading**
- [ ] `send_text` with `enter: false` types without executing; `enter: true` runs it.
- [ ] `send_text` with `paste: true` into Claude Code pastes a multi-line prompt as one message.
- [ ] `send_key` Escape interrupts Claude Code mid-answer; Shift+Tab cycles its modes; Ctrl-C interrupts a shell command.
- [ ] `wait_for_idle` on a Claude Code tab returns while Claude is waiting for input, and keeps waiting while the spinner runs.
- [ ] `get_text` matches what is on screen; negative `start_line` reaches the scrollback; `escapes: true` carries colours.
- [ ] `wezterm://pane/{id}/screen` resource shows up in the Inspector's resource list and reads correctly.
- [ ] Type into a pane yourself while the agent reads it: both see the same screen; nothing crashes when you close a pane the agent is using (the next call reports "pane N does not exist").

**Windows specifics**
- [ ] GUI discovery works over the named pipe (`steg` finds the running GUI with no flags).
- [ ] Non-ASCII in `send_text` (`"héllo → 世界"`) arrives intact in PowerShell and in Claude Code.
- [ ] A `cwd` that doesn't exist gives an error rather than a silent default.

## Not yet covered

- Windows and macOS builds in CI for the `steg` crate (wezterm's own CI
  matrix is the model; needs `get-deps` on Linux and nothing extra on
  Windows).
- Behaviour with several WezTerm GUI instances (which one `steg` picks: the
  eldest, same as `wezterm cli`).
- Push notifications and the Lua policy gate, once they exist.
