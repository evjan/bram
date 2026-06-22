---
observed: 2026-06-22
provider: claude
cli_version: 2.1.170
shape: proceed
source: screenshot
screenshot: 2026-06-22-claude-proceed-mcp-tool.png
---

# proceed (MCP tool call) — recognized; positive control for the long-command miss

The permission prompt for an **MCP tool call** (`xmlui_list_components`),
captured after removing the `mcp__xmlui` allow so it would prompt. It is a
`proceed` shape with an MCP-specific **"Tool use" preamble**, and it
**fired** (`op=fire`) — the embedded value in option 2 is short enough
not to wrap.

## Captured (stripped, from screenshot)

```
Tool use

  xmlui — xmlui_list_components (MCP)
  Lists all available XMLUI components.

Do you want to proceed?
❯ 1. Yes
  2. Yes, and don't ask again for xmlui — xmlui_list_components commands in /Users/jonudell/bram
  3. No

Esc to cancel · Tab to amend
```

## Detection axes

| axis | present | note |
| --- | --- | --- |
| cursor (`❯`) | ✓ | before option 1 |
| header (`Do you want`) | ✓ | `Do you want to proceed?` (below the Tool-use preamble) |
| 1./2. pair | ✓ | 3 options; the `2.` token is intact — option 2's embedded value is moderate length and does **not** wrap/interleave |
| footer | ✓ | `Esc to cancel · Tab to amend` — **no `ctrl+e to explain`** (bash proceed has it; MCP omits it) |
| keyword guard | ✓ | `proceed` |
| **result** | **op=fire** | recognized cleanly |

## Notes

- **MCP-specific "Tool use" preamble.** Above `Do you want to proceed?`
  there's a header block: `Tool use` / `<server> — <tool> (MCP)` /
  `<tool description>`. This is the MCP analogue of the bash command
  preview. It does not break detection (header/cursor/pair still anchor),
  but it's the distinguishing marker that a `proceed` was triggered by an
  MCP tool rather than a bash command or a file edit.
- **Option 2 is MCP-scoped:** `Yes, and don't ask again for <server —
  tool> commands in <project dir>` — same role as bash's "don't ask
  again for `<command>`", but the embedded value is the tool identity +
  project dir, not an arbitrary command.
- **Footer variant:** `Esc to cancel · Tab to amend` only — the
  `· ctrl+e to explain` tail that bash `proceed` carries is absent here.

## Relation to siblings

- **Positive control for `2026-06-22-claude-proceed-long-command.md`.**
  Identical 3-option structure, but that specimen's option 2 embedded a
  long bash command that wrapped and **interleaved** into the option text
  (`2Yes, and : PLAYWRIGH don't ask T_USE_DEV_S …`), corrupting the `2.`
  token and defeating the `1./2. pair` axis → never fired. Here the
  embedded value is short → no interleave → `op=fire`. The variable that
  decides fire-vs-miss is the **embedded-value length**, not the shape.
- Confirms **MCP tool-call permissions are the `proceed` shape** (plus the
  Tool-use preamble) — no separate "MCP" catalog row needed; annotate the
  `proceed` row in `../pty-menu-shapes.md` to list MCP tool calls as a
  trigger and note the preamble + footer variant.
