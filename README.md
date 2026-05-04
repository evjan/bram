# xmlui-desktop

A Tauri-based **workspace for XMLUI development with AI agents**.

- **Left pane** — a real terminal, where you run an AI coding agent
  (e.g. `claude` or `codex`).
- **Right pane** — the XMLUI app you're developing, served from a
  static local server out of `app/`.
- **File watcher** — as the agent rewrites XMLUI markup under `app/`,
  the right pane reloads automatically. No manual refresh.

The two panes can also talk: XMLUI components in the right pane post
text back into the terminal via `window.toShell` / `window.toTurn`
helpers (defined in `app/index.html`), so buttons, selects, and forms
can become input to whatever agent is running on the left.

See [`CLAUDE.md`](./CLAUDE.md) for the conventions Claude Code follows
when driving the right pane.

## Build

The frontend in `app/` is static — no bundler, no `package.json`. The
only build step is the Tauri/Rust build.

From `src-tauri/`:

- **Dev:** `cargo run` (or `cargo tauri dev` with the Tauri CLI)
- **Release:** `cargo tauri build`

Tauri docs: <https://tauri.app/develop/>, <https://tauri.app/distribute/>.

## Layout

- `app/` — static frontend served to the right pane (the XMLUI app
  under development lives in `app/right/`)
- `src-tauri/` — Rust backend (PTY for the terminal, custom `xmlui://`
  URI scheme, filesystem watcher, IPC handlers)
- `scripts/` — auxiliary scripts
