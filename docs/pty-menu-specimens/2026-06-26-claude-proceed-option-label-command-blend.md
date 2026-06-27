---
observed: 2026-06-26
provider: claude
cli_version: 2.1.179
shape: proceed (Bash/tool, 3-option)
source: screenshot
screenshot: 2026-06-26-claude-proceed-option-label-command-blend.png
---

# `proceed` 3-option Bash menu — option-2 label blended with the command box

A genuine, correctly-*detected* menu that **mis-rendered**: the standard
3-option Bash `proceed` shape (see `../pty-menu-shapes.md` Family A), but
option 2's label came through corrupted, with the wrapped command echo
bleeding into it.

## What was on screen

The triggering command was a single compound line, wrapped to a narrow
column inside the "Bash command" box:

```
Bash command
  jq '.items[].id' /Users/jonudell/bram/reso
  urces/worklist.json; echo "--- HEAD ---";
  git -C /Users/jonudell/bram log --oneline
  -2
  Verify worklist and git log

Do you want to proceed?
❯ 1. Yes
  2 Yes, and don't as: git -C
    again for          /Users/jonudell/bram log
                       --oneline -2
  3. No

Esc to cancel · Tab to amend · ctrl+e to explain
```

Option 2 should read **`Yes, and don't ask again for similar commands in
/Users/jonudell/bram`**. Instead the extracted label is mangled:

- `ask` clipped to `as`,
- the `2.` lost its period (`2 Yes…`),
- big interior whitespace gaps (`again for          /Users/jonudell/bram log`),
- the command tail `git -C /Users/jonudell/bram log --oneline -2`
  interleaved *into* the option text.

## Detection axes

All Family A `proceed (Bash/tool, 3-option)` axes matched — this was **not**
a detection miss: **cursor** ✓ (`❯` on option 1), **header** ✓
(`Do you want to proceed?`), **1./2. pair** ✓, **footer** ✓
(`Esc to cancel · Tab to amend · ctrl+e to explain`). So the shape fired;
the failure is downstream in **option-label extraction / render**.

## Significance — option-label/command-box column blend

Same staleness/blend *family* as
`2026-06-22-claude-quick-succession-render-blend.md`, but a different
mechanism:

- The 2026-06-22 case blended **two successive menus** (new header, stale
  options) at the Transcript render layer.
- This case blends **one menu's option label with its own command-box
  echo**: when the command box wraps to a narrow column, the grid rows
  carrying the wrapped command and the rows carrying the wrapped option-2
  label occupy overlapping horizontal regions, and the grid reader
  concatenates them into one corrupted option string.

The discriminating signal is the wrapped command tail appearing verbatim
*inside* the option-2 label (`don't ask again for` … `git -C … --oneline -2`).
A clean read would carry only `…similar commands in <dir>`.

## Out of scope — wrapping artifact

Per the width assumption at the top of `../pty-menu-shapes.md`, this is **not
a bug to chase**. The corruption is a consequence of the command box wrapping
to a narrow column, not a parsing defect; the fix is a sane terminal width,
not teaching the scanner to reconstruct un-wrapped text. Filed as the
canonical specimen of that boundary so a future narrow-column option-label
corruption is recognized as the same width artifact, not re-investigated from
scratch.
