---
observed: 2026-06-22
provider: claude
cli_version: 2.1.170
shape: (render-observation, not a CLI shape)
source: screenshot
screenshot: 2026-06-22-claude-quick-succession-render-blend.png
---

# Quick-succession inline-menu staleness (Transcript render layer)

**Not a CLI menu shape** — filed here to keep menu evidence together. This
is an **agent-pane render** bug, not a PTY-scanner detection miss.

## What happened

Two permission menus fired in quick succession:

1. the MCP `xmlui_list_components` prompt (`op=fire`, option "don't ask
   again for xmlui — xmlui_list_components commands in /Users/jonudell/bram"),
2. then the `cp` command's prompt (`op=fire`, "always allow access to
   `paste/`") — raised because filing the MCP specimen copied a screenshot
   out of `paste/`.

`pty-menu-changed` emitted twice (~4 s apart) for the transition. **Both
menus were detected** — the scanner did not miss.

But the Transcript rendered a **blend** across the A→B swap: the header
read "Agent wants to use **Bash**" (correct for the current `cp`), while
the *options* still read "don't ask again for **xmlui — xmlui_list_components**"
(the **previous** MCP menu). The terminal pane simultaneously showed the
real current `cp`/`paste/` menu. So the inline menu did not cleanly swap —
new header, stale options.

## Significance

- A **live instance of "two menus in quick succession"** (the hypothesis
  the historical-log survey found no evidence for). The failure is at the
  **render layer** (`agentMenuEvt` → `AgentMenuView` not re-keying cleanly
  on a menu→menu transition), **not** at PTY detection (both fired).
- Same staleness family as the menu-preemption bug — the inline menu
  lagging the real terminal state — here as a *wrong* menu rather than a
  *missing* one.
- Self-induced by the cataloging loop: filing the MCP specimen (`cp`
  reading `paste/`) raised the second prompt right behind the first.

## Open

Root cause unconfirmed: whether the menu object updated partially (new
kind/header, stale options) or the render didn't re-key. Would need a
focused look at the `agentMenuEvt` → `AgentMenuView` update path or a live
re-trigger. Parked per direction to keep moving.
