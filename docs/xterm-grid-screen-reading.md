# Reading the terminal screen from xterm.js instead of re-parsing the PTY stream

Status: validated by spike (branch `xterm-grid-screen-read`, 2026-06-23).
Not yet implemented beyond the spike.

## Summary

Bram's Rust host keeps a rolling ~8 KB buffer of recent PTY output bytes
(`PTY_TAIL` / `pty_tail_cell`), runs `strip_ansi` over it, and hand-parses
the result to detect the permission menu, the agent status line, the
end-of-turn banner, the `⏺ Tool(...)` signature, and more. That parsing is
fragile — and the fragility is **structural**, not incidental.

Meanwhile the frontend already renders the *same bytes* through **xterm.js**,
a full terminal emulator that holds a clean 2-D screen grid. We have never
read text out of that grid. A spike proved the grid yields clean, complete
screen text exactly where the strip_ansi parse yields mangled fragments.

The conclusion: **stop re-deriving the rendered screen in Rust from a
de-positioned byte stream; read it from the emulator we already run.** The
host keeps everything that *isn't* screen text (timing, emission, JSONL
correlation).

## Why strip_ansi mangles

`pty_tail` is a **time-ordered byte stream of the TUI's repaints**. The
terminal turns that stream into a 2-D grid using cursor-positioning escape
codes ("move to row 12 col 3, write `o`, move back, overwrite with a spinner
frame") hundreds of times a second. `strip_ansi` (`lib.rs:4928`) **deletes
exactly those positioning codes**. The visible glyphs then collapse together
in *stream* order rather than *screen* order, interleaved with spinner-
animation frames. Two consequences:

- Fragments out of order, jumbled with `✢ ✳ ✶ ✻ ✽` spinner glyphs and
  repeated `thinking` — e.g. the word "Flowing" appears as `F\nl\no\nw…`.
- Hard-wrap boundary spaces are lost (`menu box, not` → `menu box,not`).

`strip_ansi` itself has Windows-gated arms that try to *reconstruct* a
fraction of the grid (re-emit `\n` for CSI cursor-row moves, a space for
column moves) — i.e. it is hand-reimplementing a sliver of a terminal
emulator, and its own comments admit it is "platform-specific and
incomplete." The end-of-turn banner parse is even **irrecoverably lossy**:
`1m 22s` reconstructs as `1m 2s` because the missing digit "lived in the
prior screen state we can't recover."

## The unused asset

`app/main.js:32` creates `const term = new Terminal({... allowProposedApi:
true ...})`; PTY chunks are written in via `term.write` (`:564`). xterm.js
resolves every repaint/overwrite/animation into a clean grid. Its content
API is **entirely unused today** — `main.js` reads only scroll geometry
(`buffer.active.viewportY/baseY`), never a line of text. Everything needed
is present in the vendored bundle:

- `buffer.active.getLine(n).translateToString(trimRight)` — clean rendered
  text of grid row *n*.
- `line.isWrapped` — soft-wrap continuation flag (see caveat below).
- `buffer.active.cursorY / baseY / length` — geometry.
- `term.registerMarker(offset)` — pin a row.
- `term.onWriteParsed` — buffer-current notification (fires after a write
  chunk is parsed; renderer-independent, so it works even when the terminal
  pane is `display:none`).

Outbound channels are already wired: `invoke(...)` / `fetch("/__...")` to
the host, `postMessage` to the agent-pane iframe.

## What the host re-derives today (inventory)

Three buckets (full inventory in commit history / research findings):

- **(A) Screen-derived — the migration target.** ~18 load-bearing parsers,
  ~1,400–1,600 lines plus thousands of lines of Windows/Mac test fixtures.
  Menu detection + option parsing (`parse_menu_options`,
  `split_collapsed_option_lines`, `label_has_inner_numbered_marker`,
  `codex_tail_has_third_option_marker`, the anchor scanners,
  `pty_menu_detect`), the status line + substate (`parse_claude_status_line`,
  `parse_claude_status_verb_only`, `parse_substate_phrase_scan` + the
  sticky-verb/sticky-substate caches that exist only to mask eviction
  flicker), the end banner (`parse_claude_natural_end_banner`), and the
  `⏺` signature (`extract_claude_command_signature`,
  `extract_codex_command_signature`). All of these reconstruct "what is on
  screen" and all fight the stream/grid mismatch.
- **(B) Stream/timing-derived — stays host-side.** The spinner-cadence
  end-of-turn machine (`pty_agent_turn_update`, glyph/silence detectors).
  Depends on byte timing, not rendered text. xterm.js does not replace it.
- **(C) JSONL/state-derived — stays host-side.** `lookup_pending_tool_call`,
  the JSONL completion deciders, turn stats. The authoritative truth for
  completion and tool signatures.

## Spike validation (2026-06-23)

`main.js` was instrumented to read the grid above the menu and log it on
menu appearance (`subkind=xterm-grid-spike`). Two captures, both clean:

1. **Prose.** The same content that came out of strip_ansi as hundreds of
   lone spinner glyphs and `F\nl\no\nw` fragments read back as fully
   readable wrapped prose, plus a pristine menu box.

2. **Menu structure (the hardest case).** A live three-option permission
   menu — including the **multi-line wrapped option 2** ("…always allow
   access to Desktop/ from this project") — read back complete and in
   order:

   ```
   Do you want to proceed?
   ❯ 1. Yes
     2. Yes, and always allow access to Desktop/
       from this project
     3. No
   Esc to cancel · Tab to amend · ctrl+e to explain
   ```

   This is exactly the input that `split_collapsed_option_lines` and
   `codex_tail_has_third_option_marker` exist to fight (#187
   "options collapsed onto one line", "missing 3rd option"). The grid got
   it right with zero heuristics.

## Residuals (cosmetic / filtering — not blockers)

- **Hard-wrap joins persist** (`box,not`, `2.Yes`). Claude **hard-wraps its
  own output** rather than letting the terminal soft-wrap, so `isWrapped` is
  usually `false` and can't rejoin perfectly. Cosmetic; for a transient
  preview, display wrapped as the terminal does. Far milder than strip_ansi
  mangling.
- **Occasional status rows leak in** (`✽ Flowing… (1m 17s …)` glued to a
  prose row's tail). Filter with the recognizers we already have.
- **Region targeting.** The spike's anchor was a loose substring scan that
  also matched scrollback menus and its own source shown on screen. The real
  extractor must target the **live** menu near the cursor with a proper
  shape (header + `❯ 1.` rows), not a substring.

## Risk

Not capability — **event timing + visibility.** Read on `onWriteParsed`
(buffer-current, renderer-independent), never `onRender` (frame-lagged, can
go quiet when the terminal pane is `display:none` and WebGL drops its
context). The buffer model stays readable while hidden; the event choice is
the thing to keep correct.

## Architecture & migration path

The screen text lives in the **frontend** (`main.js`, the only place with
`term`); menu detection + event emission live in the **host**; the
transcript lives in the **iframe**. So a migrated signal flows: frontend
reads the grid → reports clean text to host/iframe → host keeps owning
timing, emission, and JSONL correlation. That cross-layer hop is the real
work, not the text extraction.

Recommended sequence:

1. **Pilot: the menu-stack fix** (`menu-stack-pty-inflight-prose`). Read the
   in-flight assistant prose above `⏺` from the grid, show it transiently
   above the inline menu, reconcile to the JSONL record by
   `toolCallSignature`. First real feature on the grid path.
2. **Migrate screen-derived detection item by item** — menu detect/options,
   then status line, then banner — each deleting strip_ansi heuristics in
   favor of a grid query, retiring the matching fixtures.
3. Leave buckets B and C (timing, JSONL) host-side throughout.
