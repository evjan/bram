---
observed: 2026-06-18
provider: codex
cli_version: unknown
shape: codex-action-required
source: screenshot
screenshot: 2026-06-18-codex-action-required-sudo-chown.png
trace_lines: [31548, 31550, 31552, 31554, 31556, 31558, 31560, 31562, 31564, 31566, 31568, 31570, 31572, 31574, 31576, 31578]
---

Codex `Action Required` permission menu captured while Codex requested
approval to run `sudo chown root /Users/jonudell/Desktop/foo.bar`.

Stripped on-screen text:

```text
Would you like to run the following command?

Reason: Do you want to allow changing
/Users/jonudell/Desktop/foo.bar so it is owned
by root?

$ sudo chown root
/Users/jonudell/Desktop/foo.bar

> 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that
     start with `sudo chown root /Users/jonudell/
     Desktop/foo.bar` (p)
  3. No, and tell Codex what to do differently
     (esc)
```

## Detection axes

| axis | present | note |
| --- | --- | --- |
| cursor (`>`) | effectively ✓ | screenshot shows `>` before option 1; trace `cursor=false` because this is the Codex path, not the Claude `❯` anchor |
| header (`Do you want`) | ✓ | appears in the reason text |
| 1./2. pair | ✓ | trace has `numbered=true`, `needle2_after_anchor=true`, `anchor_distance_ok=true` |
| footer (`Esc to cancel` …) | ✗ | Codex uses `(esc)` in option 3 instead of Claude's footer |
| codex action | ✓ | trace has `codex_action=true` |

## Trace evidence

`resources/bram-traces/bram-trace.log` recorded repeated `op=fire`
lines for this menu. Representative line:

```text
[pty-menu-scan] op=fire ... numbered=true needle2_after_anchor=true header=true anchor_distance_ok=true codex_action=true ... excerpt='Do you want to allow changing/Users/jonudell/Desktop/foo.bar so it is ownedby root?$sudo chown root/Users/jonudell/Desktop/foo.bar› 1. Yes, proceed (y)2.Yes,anddon’taskagainforcommandsthatstartwith`su'
```

## Notes

- Bram **did detect** this menu: `op=fire`, not a missed
  `menu_bearing=true` skip.
- `scripts/pty-menu-scan-report.py` currently reports the fired menu as
  `unknown` because its classifier only matches excerpt text, and the
  excerpt starts at the reason/command region rather than preserving the
  `Action Required` title. The next classifier fix should map
  `codex_action=true` to `codex-action-required`.
- This specimen is the first durable Codex Family A example in the
  corpus.
