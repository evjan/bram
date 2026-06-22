---
observed: 2026-06-22
provider: claude
cli_version: 2.1.170
shape: proceed
source: trace-excerpt
---

# proceed (Bash/tool) — long-command variant that never fires

A `Do you want to proceed?` Bash-permission prompt whose **option 2**
(`Yes, and don't ask again for <command>`) embeds a long command. The
catalog's `proceed` row (`../pty-menu-shapes.md`) is characterized from a
*short* command (`Run ` + `` `ls -la` ``); this long-value variant
**never produces `op=fire`** — every repaint frame is captured garbled
and skipped.

Trigger this session: the permission prompt for
`PLAYWRIGHT_USE_DEV_SERVER=false npx playwright test xmlui/src/components/List/List.spec.ts -g "scroll event" …`
while running the List scroll tests.

## Captured (stripped, verbatim from `[pty-menu-scan] op=skip menu_bearing=true`)

```
Do you want to proceed? ❯ 1. Yes 2Yes, and : PLAYWRIGH don’t ask T_USE_DEV_S again for ERVER=false npx playwright test xmlui/ src/compone nts/List/Li st.spec.ts -g "scroll event" --pr oject=xmlui -non
```

```
Do you want to proceed? ❯ 1. Yes 2Yes, and : PLAYWRIGHT don’t ask _USE_DEV_SER again for VER=false npx playwright test xmlui/s rc/component s/List/List. spec.ts -g "scroll event" --list 3. No Esc to c
```

## What it should have been

```
Do you want to proceed?
❯ 1. Yes
  2. Yes, and don't ask again for PLAYWRIGHT_USE_DEV_SERVER=false npx playwright test … this session
  3. No
Esc to cancel · Tab to amend · ctrl+e to explain
```

## Mechanism

The long command **wraps**, and its wrapped continuation is painted in
the **same screen rows** as the static option label, at overlapping
columns. The scanner reads ANSI-stripped bytes in left-to-right /
top-to-bottom screen order, so the two text runs are **spliced
column-by-column**:

- `"2. Yes, and don't ask again for"` (the label) interleaved with
- `"PLAYWRIGHT_USE_DEV_SERVER=false"` (the command)

→ `2Yes, and : PLAYWRIGHT don't ask T_USE_DEV_S again for ERVER=false`.
Note `2.` loses its period (the column where `.` would sit is overwritten
by a command char), so even the option token is corrupt.

## Axis analysis (against `../pty-menu-shapes.md`)

| axis | result | why |
| --- | --- | --- |
| `cursor` (`❯`) | ✓ | present and intact |
| `header` (`Do you want`) | ✓ | present and intact |
| `footer` (`Esc to cancel …`) | ✓ | present (when the frame reaches it) |
| **`1./2. pair`** | **✗** | the splice corrupts the `2.` token (`2Yes`) and balloons/pollutes the bytes between the `1.` anchor and a clean `2.`/`3.`, so `needle2_after_anchor` + `anchor_distance_ok` fail |
| `keyword guard` | ✓ | `proceed` keyword still matches |
| `menu_bearing` | true | which is why the skip carries this `excerpt=` |

So in catalog terms: **`proceed` regresses on the `1./2. pair` axis when
the embedded runtime value is long.** `header`/`cursor`/`footer` all hold;
the pair axis is the single point of failure.

## Relation to sibling captures

- **Short `proceed`** (`❯ 1. Yes 2. No`) — fires cleanly; the `2.` sits
  right after `1.`, no embedded value to wrap. 52 `op=fire` this session.
- **`edit` / `create`** (3-option, `2. Yes, allow all edits in <dir>`) —
  usually fire, but some captures show *dropped characters*
  (`2.Yes, llow all dits`) from the same mid-repaint splicing. Lower
  severity because the embedded value (a short dir) rarely wraps far
  enough to displace `3.`, so the pair axis still matches — but it flags
  a **capture-integrity** concern orthogonal to the structural axes: a
  frame can fire while its option *text* is corrupt, which matters when
  rendering option labels for the click-to-answer affordance.

## Follow-up

- Annotate the `proceed` row in `../pty-menu-shapes.md` with this
  long-value regression.
- Consider a capture-integrity / frame-stability axis (detect a
  mid-repaint splice; prefer a settled frame) — see the `edit`
  dropped-character note above.
