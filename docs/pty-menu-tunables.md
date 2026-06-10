# PTY menu survival tunables (host-side)

Two constants in `src-tauri/src/lib.rs` govern how long the host
keeps a detected permission menu visible to the agent pane after
the menu's bytes scroll past the detector's view in the PTY tail.
Both were tuned in response to incident #182 #17, where a Bash
permission menu was dismissed host-side 37 seconds after detection
while Claude's TUI was still presenting it on the terminal —
leaving the user with a prompt in the terminal and nothing
actionable in the agent pane.

## The constants

### `MENU_EVICTION_GRACE_MS` (60_000 — 60 s)

How long `pty_menu_update` defers the
`state=dismissed reason=buffer-evicted` emit after
`pty_menu_detect` stops finding the menu signature in the current
PTY tail. Any TUI redraw that brings the menu bytes back into the
tail within the grace clears the grace via the
`detected.is_some()` branch and keeps the menu visible. If no
redraw arrives within the grace window, the host emits the
dismiss legitimately.

Previously 1500 ms, which dismissed the menu host-side whenever a
user paused for more than ~1.5 s of TUI silence — far too short
for real "walk back to the keyboard" delays.

### PTY tail cap (65_536 bytes — 64 KB)

The rolling byte history `pty_menu_update` retains and scans
(`tail.len() > 65536` trim in `pty_menu_update`). Larger tail
means more chances for the menu signature to stay in scan range
across partial TUI redraws (spinner animation, status-line
updates) that don't re-emit the menu region.

Previously 8 KB. Byte-pattern scan at 64 KB is still sub-millisecond,
so the cost is the extra memory (~56 KB per Bram process).

## Companion: post-click suppression

`pty_menu_suppressed_cell` blocks re-detection of a tool for a
window after the user dismisses its menu by keystroke — without
this, the just-dismissed menu's bytes still sitting in the (now
larger) tail would re-trip detection.

The suppression duration is paired with the tail cap: bumped from
2 s to 10 s when the tail grew from 8 KB to 64 KB. Roughly, the
suppression should cover the time it takes for the dismissed-menu
bytes to scroll off the new tail under typical PTY chunk rates.

## Diagnosing menu-survival issues

If a permission menu disappears from the agent pane prematurely
despite the terminal still presenting it, grep the trace:

```bash
LOG=resources/bram-traces/bram-trace.log
grep -E 'state=shown|state=hold-start|state=holding|state=hold-expired|state=dismissed' "$LOG"
```

Read the sequence around the dismissal:

- `state=shown … reason=byte-pattern` — detector found the menu;
  this is the moment the agent pane should render it.
- `state=hold-start … reason=buffer-evicted grace_ms=60000` — the
  current chunk doesn't contain the menu; deferring dismiss.
- `state=holding … elapsed_ms=… grace_ms=60000` — a subsequent
  chunk still doesn't contain it; still deferring.
- `state=hold-expired … reason=buffer-evicted elapsed_ms=…` —
  grace ran out; the next line will be `state=dismissed`.
- `state=dismissed … reason=user-input` — user pressed a key, host
  cleaned up. **Distinct from `reason=buffer-evicted`** — the
  former is a real dismissal, the latter is the host giving up on
  a still-presented menu.

If `state=dismissed reason=buffer-evicted` fires in a scenario
where the user is still looking at the menu on the terminal, that
is the failure mode incident #182 #17 documented. Consider whether
to raise `MENU_EVICTION_GRACE_MS` further or whether the menu
bytes truly are no longer in the tail.

## Where the constants live

- `MENU_EVICTION_GRACE_MS`: `src-tauri/src/lib.rs` near line 1034.
- PTY tail cap (literal `65536`): `src-tauri/src/lib.rs` inside
  `pty_menu_update`, in the chunk-append trim branch.
- Post-click suppression `Duration::from_secs(10)`:
  `src-tauri/src/lib.rs` inside `pty_menu_update`, after the
  `pty_menu_suppressed_cell` lookup.

## See also

- [`docs/apis.md`](apis.md) — the HTTP-route reference, including
  `/__turn-state` which carries the live `pendingMenu` payload to
  the agent pane.
- Issue #182 incident #17:
  https://github.com/judell/bram/issues/182#issuecomment-4674505038
