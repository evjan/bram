# Bram

You are running in the **terminal** of a Tauri desktop shell that puts
a real terminal next to an XMLUI surface. The user can SEE the right
pane while talking to you. Use it.

## What to do with the target app

When the user asks for something that benefits from structured output
(tables, lists, charts, multi-line text) or structured input (selectors,
forms, multi-step flows), **edit `Main.xmlui`** (or one of the
`components/` files) so the target app renders it. A filesystem watcher reloads the iframe automatically
when you save — you do not need to ask the user to reload.

Examples:

- *"Show me my recent commits"* → write a `<Table>` or `<List>` bound to
  data you fetched, then continue the conversation pointing at it.
- *"Pick a target branch"* → render a `<Select>` whose `onDidChange`
  calls `toShell('selected: ' + value)`. The user clicks; their pick
  arrives as user input on your next turn.
- *"Walk me through this in steps"* → render a `<Stepper>` or tabbed
  `<Pages>` and let the user navigate.

## Working in XMLUI surfaces

Both panes here are XMLUI, so most edits land in `.xmlui` files. Some
rules the xmlui-standalone evaluator enforces hard:

- **No raw browser JS in event handlers** — `setTimeout`, `setInterval`,
  `fetch` outside DataSource, `async` / `await`, etc. are rejected at
  evaluation time with an unhandled rejection. Stay within App
  abstractions: `delay(ms)`, `debounce(ms, fn, ...args)`, the `Timer`
  component, `DataSource` for HTTP, `ChangeListener` for derived
  reactivity.
- **Lead with the xmlui-mcp tools** before reaching for a JS solution.
  The `xmlui_search_howto` tool is the fastest way to find the
  XMLUI-native pattern for a feature (e.g. "delay function", "debounce
  input", "wrap text in table cell"); `xmlui_component_docs` is for
  component-prop lookups; `xmlui_get_prompt` re-injects the server's
  framing guidance mid-session when you suspect you've drifted.
- **Cite a doc URL** for any non-obvious markup decision —
  `https://www.xmlui.org/docs/reference/components/<Name>` or
  `https://www.xmlui.org/docs/howto/<slug>`. If you can't cite one,
  search again.

The `xmlui-mcp` server is loaded for this conversation. Use it.

## How the target app talks back to you

The target app's `index.html` exposes helpers as window globals that
post messages to the parent shell:

| intent | from XMLUI | what the host does |
|---|---|---|
| inject text the user can edit | `toShell(text)` | text + `\n` appears in your stdin; user must press Enter |
| submit a complete user turn | `toTurn(text)` | bracketed-paste + carriage return; auto-submits as a fresh turn |
| send raw key bytes to PTY (no newline) | `sendKeys(text)` | bytes go straight to PTY stdin — for Esc, arrows, single-key menus |
| open an external URL | `openExternal(url)` | host opens the URL in the system browser |
| capture screenshot of right pane | `captureScreenshot()` | host captures + injects the file path as a `toTurn` |
| log without bothering you | `logToHost(payload)` | recorded in cargo run stderr only — invisible to you |
| open devtools | (wrench icon does it) | n/a |

Use `toTurn` for one-shot form submissions (Approve buttons, Confirm
buttons, single-pick selectors). Use `toShell` only when you want to
inject text the user can edit before sending.

```xml
<Select onDidChange="(v) => toTurn('branch: ' + v)">
  <Option value="main" label="main" />
  <Option value="dev"  label="dev" />
</Select>

<Button label="Confirm" onClick="toTurn('confirmed')" />
```

The user types or clicks; you receive `branch: dev` (or whatever you
chose) as a fresh user message.

## Coordinating via worklist.json (canonical worklist)

`resources/worklist.json` is the canonical surface for coordinating
multi-step work between you and the user. The Worklist tab in the
agent pane renders it under the heading "Worklist". Use it
whenever you'd otherwise enumerate small, independently-approvable
changes in prose.

The full lifecycle (proposed → applied → committed), payload shapes,
authorization flow (`/__worklist/resolve`, `/__worklist/mutate`), and
edge cases live in `@app/__shell/conventions.md`, which is `@`-imported
below — read that for the authoritative description. Don't duplicate
conventions guidance here; this file points at the source of truth.

## Charting

The `xmlui-echart` extension is loaded — `<EChart>` is available
out of the box. It wraps Apache ECharts and accepts any valid ECharts
`option` configuration. Use it whenever the user asks for a chart
(line, bar, pie, scatter, heatmap, etc.). XMLUI theme colors are
applied automatically.

Reference: https://docs.xmlui.org/howto/use-echarts-for-advanced-charting
and https://echarts.apache.org/en/option.html for the full option API.

## Files you'll edit most

- `Main.xmlui` — the XMLUI surface (the one)
- `components/*.xmlui` — Workspace, Sessions, Toolbar, Architecture, etc.
- `config.json` — XMLUI app config (resources, appGlobals)
- `resources/*.svg` — custom icons; register in
  `config.json` under `resources` with the `icon.<name>` prefix
- `app/__shell/helpers.js` — window helpers loaded by `index.html` via
  `xmlui://localhost/__shell/helpers.js`

## Files to leave alone unless asked

- `src-tauri/src/lib.rs` — Rust backend (PTY, custom URI scheme,
  filesystem watcher, IPC command handlers)
- `app/main.js`, `app/index.html` — parent shell wiring
- `app/vendor/*` — vendored libraries (xmlui-standalone, xterm.js, etc.)

## Inspector

The target app mounts `<Inspector />` in the AppHeader's profile menu
slot — it's the magnifying-glass icon top-right. It shows semantic
traces of XMLUI events. Open it when you're debugging interactions
before assuming the markup is wrong.

## Architectural background

The deeper narrative — why Tauri, why a static frontend, the gotchas
we hit (Tauri's SPA fallback, XMLUI's hidden `config.json` requirement,
cross-origin iframe reload) — lives at
`~/.agents/scout/projects/claude-code-desktop.md`. Read it if a
mechanism here surprises you.

<!-- bram:start -->
@app/__shell/conventions.md
<!-- bram:end -->
