# Esc / strand / resend redesign — plan of record

## Why this document exists

On 2026-07-03, while testing the turn-transport redesign, an Esc during an
in-flight turn popped a queued outbound-turn **frame** (opaque plumbing text)
back into the terminal input. The user cleared it by hand; the bounce banner
appeared; nothing could restore the message. Verdict on approving the
transport cleanup: *"the esc/resend mechanism is a botch, needs total redo."*

The mechanism is not one design but accreted layers, each patched in against
one observed failure mode. This document names the layers, the root problems,
and the contracts for the rebuild — before any code changes, per the
turn-transport playbook (`docs/turn-transport-redesign.md`).

## Inventory: the current layers

1. **Bounce detector** (host, `lib.rs` ~7892–7959): on an agent-pane Esc,
   if the grid reported no first output AND a toTurn landed within 120 s,
   emit `message-bounced`. Observe-only; cause-anchored heuristics.
2. **First-output-seen reporter** (parent shell grid → host cell,
   `report_first_output_seen`): grid-derived boolean the bounce detector
   pairs with send recency. Observe-only groundwork (#210).
3. **Bounce banner + Resend** (iframe, `Main.xmlui` ~475–487): banner on
   `message-bounced`; **Resend sends a bare `\r`** — it re-submits whatever
   still sits in the terminal input. It does not resend any stored message.
4. **Send/esc capture scrapers** (parent shell, `main.js`): fingerprint
   terminal rows around sends and escapes (`send-capture` / `esc-capture`
   traces). Forensics only; nothing consumes them at runtime.
5. **Post-escape windows** (host): 15 s no-kill window after Esc, double-tap
   hard kill, single-Esc soft turn-end. (Turn-lifecycle semantics — mostly
   fine, and explicitly NOT the target of this redo.)
6. **Workspace landing tracking** (iframe): `awaitingResponse` +
   `submittedWorklistMessage` + baseline in localStorage, compared against
   the exchange via `__bramWorklistSubmittedMatches`, cleared by four
   separate turn-end listeners.

## Root problems

- **Recovery has no source of truth.** Resend presumes the message still
  sits in the input; Esc-restore shows the frame, not the user's words.
  Nothing recovers from the durable artifact.
- **Detection is cause-anchored, not outcome-anchored.** Each detector keys
  on a cause (Esc pressed, no first output) instead of the one question that
  matters: *did the send land as a user turn?* Causes multiply; the outcome
  is singular.
- **State is scattered** across host cells, parent-shell captures, and
  iframe localStorage, with no single record of a send's lifecycle.
- **Envelopes changed the ground truth and nothing uses it.** Every
  substantial send now has a persistent envelope with a stable id, and the
  frame carries that id into the session JSONL — landing confirmation for
  enveloped sends is an exact id match, not a text heuristic. None of the
  six layers exploit this.

## Invariants for the rebuild

### 1. One outbound-send ledger (host)

Every toTurn send gets a ledger entry at injection time: send id (the
envelope id when framed; a generated id otherwise), the collapsed inline
text or envelope ref, and a state machine: `injected → landed | stranded`.
The ledger is host-side, in one place, and is the ONLY source the UI reads.

### 2. Outcome-based landing detection (host)

The host owns the session JSONL and the turn projection. It confirms
`landed` by observing the send appear as a user turn — exact envelope-id
match for framed sends, normalized-text match for inline sends. `stranded`
is `injected` + agent settled + never landed. No Esc anchoring, no grid
heuristics in the decision; the grid signal may remain as a fast hint only.

### 3. Recovery from the envelope, never from the terminal

- **Resend** re-injects from the ledger (the frame for enveloped sends, the
  recorded text for inline sends). It must work when the input is empty.
- **Esc-restore**: when the input is left holding a frame, replace it with
  the envelope's original text (mode prefix included) so the user edits
  their words; resubmit re-envelopes.

### 4. Esc stays non-blocking

Every Esc interrupts, always. The #210 hold-gate was reverted for good
reason; the redo must not reintroduce input gating as a detection aid.

### 5. One affordance, ledger-driven

A single banner state machine driven by ledger events (injected / landed /
stranded), replacing the cause-specific `message-bounced` wiring and the
four-listener localStorage clearing dance.

## What gets deleted when the rebuild completes

- Bounce heuristics (first-output + recency pairing) as the decision maker.
- Iframe landing state: `submittedWorklistMessage`, baseline,
  `__bramWorklistSubmittedMatches`, and the localStorage awaiting keys.
- The `\r` Resend.
- Capture scrapers demoted to diagnostics or removed if the ledger traces
  supersede them.

The redo must end net-simpler, as the transport redesign did.

## Sequencing (non-negotiable order)

1. Implement the ledger + landing detection host-side, **observe-only**:
   trace lines + a Status-tab row, no UI behavior change.
2. Soak against real usage (both providers); tune until zero false
   strands / false landings.
3. Switch the banner + Resend + Esc-restore to ledger events.
4. Delete the superseded layers.

Never span old and new detection across the affordance at the same time.

## Relation to existing worklist items

`detect-stranded-unsent-message` (including the 2026-07-03 folded-in
restore scope) is **subsumed by this plan** — its detector is this plan's
phase 1, its banner is phase 3. When this plan lands, rescope that item to
"phase 1: outbound-send ledger, observe-only" or drop it in favor of fresh
phase items.

## Open questions for phase 1

- Envelope-for-all-sends: should tiny inline sends also write envelopes so
  the ledger has one recovery shape? (Cost is trivial; decide at phase 1.)
- Codex parity: landing detection reads Codex JSONL through the same
  projection; verify the id/text match holds for Codex record shapes.
- Interplay with Claude Code's own queue (queued_command attachments):
  a queued frame popped back by Esc is the case that motivated this redo —
  phase 1 must ledger queued sends distinctly enough to recognize it.
