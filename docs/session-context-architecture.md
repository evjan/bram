# Session context vs. page-local state

A design line for where state lives in Bram, written after a class of
bugs — the permission-menu "miss" and the voice "wedge" — kept recurring
because session-global state was living inside routed pages that XMLUI
unmounts on tab switch.

## First: which "shell"?

"Shell" is overloaded in Bram. This doc never uses the bare word. There
are four layers, and it matters which one we mean:

1. **Native shell (Tauri / Rust).** `src-tauri/src/lib.rs` and the Tauri
   WebView window. Owns the PTY, the agent process, turn-state
   detection, and pushes Tauri events. This is the deepest source of
   truth — it survives *everything* short of quitting the app.

2. **Host page (`__shell`).** `app/index.html`, `app/main.js`, and
   `app/__shell/helpers.js` — the parent WebView document that hosts the
   terminal (xterm) next to the agent-pane iframe. It bridges native ↔
   iframe and holds host-side caches and the canonical setters
   (`window.bramAgentMenu`, the latest-jsonl cache, the agent-status
   broadcast). Survives iframe reloads, not just tab switches. The
   directory is literally named `__shell`.

3. **Agent-pane App scope.** Inside the agent-pane iframe (`app/tools/`),
   the `<App>` level of `Main.xmlui` — its `External`/`DataSource`
   subscriptions, App-scope vars, the `AppHeader`, and the `Footer`.
   This is **always mounted**: it persists across every tab switch
   because it is *outside* the routed `<Pages>`.

4. **Pages (tabs).** The routed `<Page>` children — Worklist, Transcript,
   Sessions, etc. XMLUI **unmounts** the inactive page on every tab
   switch and builds a fresh instance on return (confirmed empirically).

When the rest of this doc says **"the shell owns X,"** it means layers
1–3 — the always-mounted layers — with the **native shell + host page**
(1–2) as the *source of truth* and the **App scope** (3) as the
*persistent rendering surface*. It never means a Page (4). It is not
about the Tauri native shell specifically.

## The line

> **The shell owns the agent-session context and all expensive shared
> data. Pages are disposable lenses over it. Pages own nothing but pure
> view ephemera.**

The aim is not to *preserve* pages. The aim is to make pages **cheap to
destroy and rebuild from shell state**, so that unmount-on-switch stops
being something to fight.

Three categories, and where each lives:

### 1. Session-global context → shell

State that belongs to the *agent session*, not to any one view. Source
of truth in the native shell / host page; **rendered persistently in the
App scope** (AppHeader / Footer / a persistent region), never only in a
Page:

- agent status (working / verb / finished) — already App-scope
  (`mainAgentStatus`), rendered in the Footer
- the **permission / terminal menu** ("the agent is asking you a
  question") — host-owned data (`window.bramAgentMenu`), but its *full
  view* currently leaks into the Transcript page; the Footer only shows
  a stripped indicator. This is the main offender.
- voice / dictation state (`worklistVoiceTarget`, `footerVoiceRecording`,
  `footerVoiceArrival`) — the *vars* are App-scope, but voice is **not**
  cleanly shell-owned in practice: start the mic on one tab, switch, and
  it wedges — and this reproduces even from the always-mounted Footer mic,
  so it is not just a page-unmount problem. The recorder itself is
  host-side (`MediaRecorder` in `main.js`), so something in the flow
  couples to the active page or to iframe re-render. **Open** — a real
  bug, not a settled case (see next steps).
- the composer (message-to-agent) — already in the Footer
- the inflight worklist claim — already App-scope (`headerInflightClaim`),
  shown in the AppHeader from any tab
- session identity / provider, whisper availability, enhance/setup and
  right-pane info — already App-scope

Most of this is already where it belongs. The footer and header are the
proof of the pattern; the menu's full view is the notable leak, and voice
(above) is an open exception we don't yet understand.

### 2. Expensive / shared derived data → shell cache

Data that is costly to produce and shared across lenses: the jsonl
session stream and its parsed turns, the session list. Compute **once**,
hold in the host/App-scope cache, and let whichever Page is the active
lens render *from* it.

This is the part that dissolves the performance wall. Sessions is a
"heavy lift" because it *parses on mount*. If the parsed data lives in
the shell, remounting Sessions is cheap — which is exactly why we don't
need to keep it mounted.

### 3. Pure page-local view ephemera → page

Scroll position, in-progress draft text, expanded-row sets, local
toggles. This genuinely belongs to the view, not the session. Pushing it
into the shell would bloat and couple the shell for no benefit. It is
lost on unmount **by default**, and that is usually fine; preserve it
only case-by-case.

## Consequence: disposable pages

If the shell owns categories 1 and 2, a Page can be unmounted and
remounted instantly with no data loss and no expensive recompute — it
just re-renders from shell state. Unmount-on-switch stops being a bug to
fight: don't preserve the page, own its state elsewhere (categories 1 and
2) and let the page be cheap to rebuild.

## Implications / next steps (separate items)

- **Hoist the full `AgentMenuView` to the App scope** (render it in the
  persistent region next to the Footer indicator, subscribed to
  `window.bramAgentMenu`). Fixes the menu miss structurally: the menu is
  present and answerable from any tab, and a Page never owns the only
  copy. Decide whether Transcript keeps an inline copy.
- **Audit each Page for leaked session context** against this line, and
  move anything in category 1 or 2 to the shell.
- **Diagnose the voice wedge.** It reproduces from the always-mounted
  Footer mic (start recording, switch tab, it wedges), so it is *not*
  simply a page-unmount problem — the host-side `MediaRecorder` flow
  couples to something that changes on tab switch. Find that coupling
  before deciding where voice state and the recorder should live.
- **Move expensive parsed data (sessions, jsonl turns) to a shell cache**
  so heavy pages remount cheaply.
