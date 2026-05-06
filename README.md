  # xmlui-desktop

A desktop app that pairs an AI coding agent with the XMLUI app it's building.

- **Left pane** — a real terminal, where you run an AI coding agent
  (e.g. `claude` or `codex`).
- **Right pane** — the project's XMLUI app (`Main.xmlui` at the repo
  root), served via the binary's `xmlui://` URI scheme.
- **File watcher** — as files in the project change, the right pane
  reloads automatically. No manual refresh.

The two panes can also talk: XMLUI components in the right pane post
text back into the terminal via `window.toShell` / `window.toTurn`
helpers, so buttons, selects, and forms can become input to whatever
agent is running on the left.

See [`CLAUDE.md`](./CLAUDE.md) for the conventions Claude Code follows
when driving the right pane.

https://github.com/user-attachments/assets/3d617d7a-f864-41f4-bc77-c6449a8c1bf2

## Prerequisites

xmlui-desktop opens an XMLUI app next to your terminal — you need a
project for it to open. If you don't already have one, follow
<https://xmlui.org/get-started> to scaffold one, then run
`xmlui-desktop` from its root.

## [Download the latest release →](https://github.com/judell/xmlui-desktop/releases/latest)

## Build

The frontend is static — no bundler, no `package.json`. The only build
step is the Tauri/Rust build.

From `src-tauri/`:

- **Dev:** `cargo run` (or `cargo tauri dev` with the Tauri CLI)
- **Release:** `cargo tauri build`

Tauri docs: <https://tauri.app/develop/>, <https://tauri.app/distribute/>.

## Layout

- `Main.xmlui`, `components/`, `resources/`, `manual.md`, `Globals.xs`,
  `config.json`, `index.html` — the XMLUI app at the repo root.
- `app/` — parent shell (Tauri webview entry, terminal wiring, vendor
  scripts, and `__shell/helpers.js` that the right pane includes).
- `src-tauri/` — Rust backend (PTY for the terminal, custom `xmlui://`
  URI scheme, filesystem watcher, IPC handlers).
- `scripts/` — auxiliary scripts.

## Working with a real backend

xmlui-desktop binds the right-pane HTTP server to
`127.0.0.1:<random-port>` (it uses port `0` and lets the OS pick).
That's fine for projects that talk only to public APIs or static
files. It breaks when your project needs a **fixed origin** — OAuth
callbacks, CORS allowlists, hardcoded API base URLs.

> **Compatibility note.** The right pane is an iframe. Backends that
> send `X-Frame-Options: DENY` or `Content-Security-Policy:
> frame-ancestors 'none'` (common for security-sensitive admin UIs)
> cannot be loaded into the right pane regardless of port. For those
> projects, open them in a standalone browser instead.

### The redirect pattern

Run your frontend on a known port in a separate terminal:

```
python3 -m http.server 8080
```

Add a self-redirect at the top of your project's `index.html`:

```html
<script>
  if (location.hostname === '127.0.0.1' && location.port !== '8080') {
    var devQuery = location.search || '?defaultParam=value';
    location.replace('http://localhost:8080' + location.pathname + devQuery + location.hash);
  }
</script>
```

Launch `xmlui-desktop` from the project root. Its iframe loads the
random-port URL once, your script bounces it to `localhost:8080`, and
your fixed-origin bindings line up.

### URL parameters

Use query strings to parameterize the frontend without rebuilding —
e.g. `?city=santarosa` to switch tenant. The redirect above preserves
whatever `?key=value` you launch with, or supplies a default when
launched without one.

### Working example

[community-calendar](https://github.com/judell/community-calendar) uses
this pattern for GitHub-OAuth-via-Supabase. See `xmlui/index.html` for
the redirect snippet and
[`docs/app-architecture.md`](https://github.com/judell/community-calendar/blob/main/docs/app-architecture.md)
for the Supabase URL-Configuration setup that requires the fixed
`localhost:8080/**` origin.
