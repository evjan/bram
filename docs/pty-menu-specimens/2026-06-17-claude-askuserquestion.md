---
observed: 2026-06-17
provider: claude
cli_version: 2.1.169
shape: askuserquestion
source: screenshot
screenshot: 2026-06-17-claude-askuserquestion.png
---

Claude Code *AskUserQuestion* prompt (Family B) — captured while
iterating on the #197 worklist item. Stripped on-screen text:

```
◆ Item structure
This item now bundles the skill fix, the capture instrumentation, and
the catalog doc. How do you want it structured?
❯ 1. Keep as one item
    One worklist item / one commit covering skill fix + capture
    instrumentation + catalog. Simplest to review as a coherent
    whole, larger diff.
  2. Split into two
    Item A: catalog doc + skill-prompt fix (the concrete coverage
    win). Item B: the capture instrumentation (excerpt + menu_bearing).
    Smaller, independently reviewable commits.
  3. Split into three
    Separate items for (1) skill fix, (2) catalog doc, (3) capture
    instrumentation. Maximally granular; more triage overhead.
  4. Type something.
  5. Chat about this
```

## Detection axes

| axis | present | note |
| --- | --- | --- |
| cursor (`❯`) | ✓ | before option 1 |
| header (`Do you want`) | ✗ | header is the *question*, variable text |
| 1./2. pair | ✓ | numbered options, ≤512 bytes apart |
| footer (`Esc to cancel` …) | ✗ | no permission footer |
| keyword guard | ✗ | none of `do you want / approve / permission / …` |

## Notes — how it differs from Family A

- The header is the **question text**, not a fixed `Do you want…`
  fragment, so the header axis cannot anchor it.
- A `◆`/`◇` diamond marks the question line; options 4–5 are a free-text
  escape ("Type something") and a back-to-chat option ("Chat about
  this") rather than Yes/No permission choices.
- With cursor + numbered pair present but header/footer/keyword-guard
  absent, the current `pty_menu_detect` would **skip** this (and the
  `[pty-menu-scan]` trace should now show `menu_bearing=true` via the
  numbered-pair branch). That's the right signal that Family B needs its
  own detection path — the follow-up tracked in `../pty-menu-shapes.md`.
- Surfacing would route the numbered choice via `sendKeys`; the "Type
  something" escape via `toShell`.
