# Handoff to Codex — 2026-06-10 07:15 UTC

Jon is benching Claude. This is what's loaded and ready for you to
pick up.

## What just shipped (head of `main`)

```
e454909  dedupe-turn-state-emits: suppress source-only re-emits
a785f29  instrument-pty-menu-byte-pattern-scans: surface detector decisions
9391baf  reactive-voice-target-reset: ChangeListener + extract inflight handler
9c5d318  finish-tool-uses-bridge-across-inflight-clear: keep section mounted
4d39afe  route-voice-logs-into-bram-trace: surface voice + voice-host
1650c19  reset-voice-target-on-inflight-clear: drop the trapped voice target
b54e9b1  stabilize-tool-uses-source-alternation: keep section mounted
5bb2f19  Add host turn-state spine                (amended for pty-menu priority)
c3bdea0  clipboard-image-paste-into-message-agent: end-to-end screenshot attach
```

Bram is currently running the binary built at `2026-06-10 07:00:08`,
process started `07:09:34`. That includes everything through
`e454909`. New trace lines visible in `bram-trace.log` include
`[pty-menu-scan]`, `[voice]`, `[voice-host]`, and `[turn-state]`
ending with `emitted=true|false`.

## What's instrumented now (and what each line tells you)

- `[pty-menu-scan] op=fire|skip stripped_len=… cursor=… numbered=…
  needle2_after_anchor=… needle2_anywhere=… header=…
  anchor_distance_ok=… codex_action=… fp_head=… fp_tail=…` —
  sampled at 5 Hz at every `pty_menu_detect` call (`src-tauri/src/lib.rs`
  near `pty_menu_update`). Tells you which anchors fired or didn't
  fire when. Use this to diagnose detector misses.
- `[turn-state] from=X to=Y … emitted=true|false` — every spine
  source-of-truth change, with whether it produced a Tauri emit.
  Use `emitted=false` lines to count suppressions.
- `[voice] stage=…` and `[voice-host] stage=…` — full voice pipeline
  from `voiceStart` → `mediaRecorder-onstop` → `whisper-request` →
  `deliverTranscript` → `voice-into-result`. Every stage emit is now
  in the trace; the failing stage of any voice failure is whichever
  stage doesn't appear.

## Verified-clean state

- Voice transcript leak (target stuck at `'feedback'` after
  inflight-clear, transcripts dropped). Fixed at `1650c19` + the
  reactive ChangeListener in `9391baf`. The fix is "reset target to
  message-agent whenever `selected || feedbackExpanded` is false."
- Tool-uses section flashing during turn-boundary source alternation.
  Fixed at `b54e9b1`, follow-up cleanup at `9c5d318`. Sticky cache is
  the cross-turn bridge; do not re-introduce sticky resets on
  submit or inflight-clear without understanding the regression.
- Menu appearing-then-vanishing race. Fixed when `5bb2f19` was
  amended so `pty-status-fallback` doesn't clobber `pending_menu`.
  Verified 2026-06-10 05:18, 05:22, 06:48.
- `kind=turn-state-changed` Tauri emit storm. Tightened today in
  `e454909`. Storm volume halved; `/__turn-state` GET rate dropped
  ~4×; dismiss delay improved 881–1969 ms → 370 ms.

## Open issues you should consider before touching menu plumbing

**Render delay for `pty-menu-changed` is still ~482 ms on a clean
post-fix sample.** `turn-state-changed` itself delivers in 2–3 ms,
so the host emit path is fine. The remaining gap is **iframe-side**:
the iframe processes the `turn-state-changed` ChangeListener first
(it's a tighter listener), and its cascade (6 `/__turn-state` GETs
per menu show, plus a `/__sessions/latest-tail` GET) keeps the main
thread busy until the cascade settles. `pty-menu-changed` and
`talk-session-changed` queue behind that.

Two follow-up items worth considering:

1. **`debounce-iframe-refetch-on-turn-state-changed`** — the 6 GETs
   per menu show are redundant; each fetches the same
   `/__turn-state`. A 50 ms debounce on the refetch would collapse
   to 1 GET and unblock the main thread sooner.
2. **`prioritize-pty-menu-changed-listener-order`** — if the iframe
   registered `pty-menu-changed` *before* `turn-state-changed`, the
   menu render might land first regardless of cascade. Unverified
   that listener registration order affects Tauri delivery; would
   need a small test.

Either is small. (1) is more obviously right; (2) is speculative.

## Open #182 incidents (current state of the umbrella)

- **#11 agent-status-stuck-finished-recurrence-no-slash-commands-2026-06-10**
  — open. Recurrence of the #2/#3 diagnostic signature. The PTY
  parser silently stops emitting `state=working` while JSONL
  continues to record `non-final-assistant`. The fix is not in this
  session; #11 currently waits for either a fresh PTY-parser hunt
  or a JSONL-non-final fallback emitter on the agent-status path.
- **#12 pty-menu-signature-extractor-catches-prose-and-clips-prefix**
  — open. Jon is collecting samples. First example showed a Bash
  permission menu where the signature preview started mid-string
  (`ync"` instead of the command) and ended with assistant prose
  (`Look at trace evidence of the flap`). Options parser was clean;
  bug is isolated to signature extraction in
  `lookup_pending_tool_call` / `extract_codex_command_signature`.
  If you accumulate a few more samples, instrumenting
  `signature_start` / `signature_end` / `signature_preview` on the
  `[pty-menu] state=shown` line is the natural next step
  (`instrument-pty-menu-signature-bounds`).

The #4 followup (perf-driven menu-render-desync) is *partially
resolved here* via `e454909` (storm reduction), but the remaining
~482 ms render delay is a separate iframe-cascade issue (see
above), not a recurrence of the original LoggerService firehose.

## Conventions worth knowing

The `~/.claude/projects/-Users-jonudell-bram/memory/` directory
holds memories the Claude side accumulated. The ones most relevant
to current work:

- **`feedback_instrumentation_first_for_diagnosis.md`** — Jon's
  explicit principle: "name of the game is Instrumentation, and we
  keep adding until we know everything." When a failure recurs
  without trace evidence to pinpoint the cause, propose an
  instrumentation item before a speculative fix. Worked five times
  this session; the #11 incident, #4 followup, #12 incident, the
  voice-target leak diagnosis, and the spine-storm diagnosis all
  followed this pattern.
- **`feedback_xmlui_no_double_quotes_in_attribute_comments.md`** —
  literal `"` inside a `//` comment inside an XML attribute
  (`onClick="…"`, `onDidChange="…"`) ends the attribute mid-string.
  XML tokenizer doesn't understand JS comments. Hit in this
  session at `9c5d318`; recovered by extracting the handler to
  Globals.xs in `9391baf`.
- **`feedback_xmlui_read_rules_first.md`** — rule #9 ("keep complex
  expressions out of XMLUI properties") covers more than just
  ternaries; multi-line `onDidChange` bodies belong in `Globals.xs`.

## Working tree state

`.bram.json` is dirty (settings drift, ignore it). Everything else
is clean. The handoff item `write-codex-handoff` sits TO COMMIT
covering this file; drop it from the worklist once you've read this.

## What's not done

The bench is partly because today made progress along
"instrumentation-first," and Jon wants to see the new spine survive
a real Codex turn (the `[turn-state]` `emitted=` field will tell
you how it behaves in your hands). If you start a session, the
first thing to look for: does the `/__turn-state` GET rate stay at
~12/s or does it climb back toward 48/s during anything you're
doing? If it climbs, find the new source-of-truth path that's
re-emitting.

Bram restart hot-reload is **off** (per the watcher setting Jon
flipped on `4efdca3`). Every iframe code change needs Jon to Cmd+R
the agent pane; every Rust change needs a binary rebuild + Bram
restart. Jon does the restart; you do the rebuild.

Good luck. Treat the trace as the primary surface; it's loaded
with enough signal now that almost any bug will show up there if
you grep for it correctly.
