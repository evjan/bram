---
observed: 2026-06-22
provider: claude
cli_version: 2.1.170
shape: askuserquestion
source: screenshot
screenshot: 2026-06-22-claude-askuserquestion.png
---

# AskUserQuestion (Family B) — second specimen, showing UI drift

Captured while asking how to make xmlui MCP calls prompt again (the
permission-survey turn). Same shape as the first specimen
(`2026-06-17-claude-askuserquestion.md`, cli 2.1.169) but the rendering
has **evolved** by cli 2.1.170, and the changes are detection-relevant.

## Captured (stripped)

```
□ Approach
How do you want to make xmlui MCP calls prompt again?
❯ 1. Local override (ask)
  2. Remove from team file
  3. Just show, don't change

  [side preview panel: .claude/settings.local.json snippet]

Notes: press n to add notes
Chat about this
Enter to select · ↑/↓ to navigate · n to add notes · Esc to cancel
```

## Detection axes

| axis | present | note |
| --- | --- | --- |
| cursor (`❯`) | ✓ | before option 1 |
| header (`Do you want`) | ✗ | header is the *question*, variable text |
| 1./2. pair | ✓ | numbered options, ≤512 bytes apart |
| footer (permission) | ✗ | **no permission footer** (`Esc to cancel · Tab to amend · ctrl+e to explain`) … |
| footer (nav) | **✓ (NEW)** | … but a **question-nav footer is present**: `Enter to select · ↑/↓ to navigate · n to add notes · Esc to cancel` |
| keyword guard | ✗ | none of `do you want / approve / permission / …` |

## How it differs from the 2026-06-17 sibling (drift)

- **Header glyph:** `□ Approach` (square + short header label) vs the prior
  `◆ Item structure` (diamond). The question now carries a short header
  chip distinct from the question text.
- **Footer exists now.** The first specimen concluded Family B has *no*
  footer. This capture shows a **navigation footer** —
  `Enter to select · ↑/↓ to navigate · n to add notes · Esc to cancel`.
  It is **not** the permission footer, so `line_is_menu_footer` won't
  match it — but it is a **stable, distinctive anchor** that could let us
  detect Family B without a `Do you want` header. This is the most
  useful new signal in this specimen.
- **Tail affordances are separate, not numbered.** Options are 1–3 only;
  "Chat about this" and "n to add notes" are bottom affordances, where
  the 2026-06-17 capture rendered "Type something" / "Chat about this" as
  numbered options 4–5.
- **Side preview panel** (the new previews feature) sits to the right of
  the options.

## Status

Still **skipped** by the current `pty_menu_detect` (header/footer/keyword
all absent in the permission sense). Family B detection remains a
follow-up; the new nav-footer anchor is the most promising path and is
worth capturing in the catalog's Family B notes. Refs the `askuserquestion`
row in `../pty-menu-shapes.md`.
