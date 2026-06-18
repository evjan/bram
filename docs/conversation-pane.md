# Conversation pane

The conversation pane (`app/tools/components/ConversationPane.xmlui` and its two
children) shows the latest exchange between the user and the agent. This is its
behavioral spec — the contract the implementation must hold — plus the
mechanics that make it stable, and the traces for debugging it.

## Expected behavior (the spec)

The pane has three sections, top to bottom:

1. **You** — the user's last message (text + any pasted images).
2. **Agent: Tool uses** — the tool calls of the current/last turn.
3. **Agent: Last response** — the agent's latest assistant text.

The rules:

- **You** and **Agent: Last response** stay present during an active exchange
  and never unmount; the last response holds its prior text (dimmed) across the
  turn-start gap rather than blanking. The cold-start case — no exchange at all
  — shows the single "No recent exchange to show." placeholder.
- **Agent: Tool uses is shown only when the turn has tools.** With no tools the
  whole section is omitted — no empty list, no "no data" placeholder. It
  appears when the first tool of a turn lands and rows spool in. Its list is
  about three rows tall, then scrolls.
- **At the start of a user turn:** the user message shows, and the prior
  **Agent: Last response is dimmed** (`opacity 0.55`).
- **When the agent responds:** the response **fades in** (opacity → 1 via a CSS
  transition).
- **During the turn:** new assistant text **accumulates** in Agent: Last
  response; new tool uses **spool** into Agent: Tool uses.
- **None of the above causes a screen refresh.** No section unmounts, no layout
  shift, no flash. Content changes in place.

## Why it used to flash, and the rule that prevents it

Each section used to fetch `url="/__last-exchange?t={tick}"` as its own
DataSource, and a `talk-session-changed` event bumped the tick. **Changing a
value inside the URL restarts the DataSource query**: `.value` goes `undefined`
while the new request is in flight, the `when="{ds.value && …}"` gate flips
false, and the `List`/`Markdown` subtree unmounts and remounts — the flash.

The fix follows the XMLUI DataSource model
(https://docs.xmlui.org/components/DataSource):

- **Never put a changing tick in the URL.** Refresh in place with `refetch()`
  or `pollIntervalInSeconds` on a **stable URL**. A refetch keeps the prior
  `.value` (`isRefetching`) while the new request runs.
- **Structural sharing** (on by default) keeps `.value`'s reference when the
  payload is unchanged, so a poll that returns identical data triggers no
  re-render.

## Mechanics

- **`assistantText` is the whole turn, not the last block.** A multi-step turn
  emits several assistant messages (text, tool, text, …). `read_last_exchange`
  (`src-tauri/src/lib.rs`) **concatenates** every assistant text block since the
  last user message, so "Agent: Last response" holds the entire response and
  grows as blocks land. The pane's `overflowY="auto"` container
  (`Workspace.xmlui`) makes a long response scrollable.
- **One DataSource, in the parent, refreshed by the event only.**
  `ConversationPane.xmlui` owns
  `<DataSource id="lastExchange" url="/__last-exchange" />` and a
  `talk-session-changed` ChangeListener that calls `lastExchange.refetch()`
  (debounced 250ms). **No `pollIntervalInSeconds`**: a steady 1s poll
  re-rendered the growing full-turn Markdown every tick during streaming and
  starved the shared renderer thread, which surfaced as multi-second
  `getUserMedia`/mic-start latency. The event fires on real JSONL changes, so
  the pane still updates live and accumulates; structural sharing keeps
  `.value`'s reference when nothing changed, so an unchanged refetch causes no
  re-render.
  - Cite: https://docs.xmlui.org/howto/chain-a-refetch ;
    structural sharing — https://docs.xmlui.org/components/DataSource
- **The value flows down as props.** Children receive
  `exchange="{lastExchange.value}"` and an `active` flag and read them as
  `$props.exchange` / `$props.active`. They do not fetch.
  - Cite: https://docs.xmlui.org/howto/compose-components-with-nesting
- **Sections gate on `active`, not on content.** `active =
  lastExchange.loaded && conversationActive(lastExchange.value)`
  (`Globals.xs`), which stays true across refetches, so the sections stay
  mounted while content streams in. `conversationActive` is false only when
  there is no exchange at all (cold start).
  - Cite: https://docs.xmlui.org/howto/hide-an-element-until-its-datasource-is-ready
- **Dim/fade** lives in `AgentLastResponse.xmlui`, driven by the **exchange
  itself** so it works for terminal-typed turns and worklist-composer turns
  alike. A change in `exchange.userText` means the user just spoke → dim
  (`opacity 0.55`) and capture the assistant-signature baseline. When the
  assistant signature advances past that baseline, the new response arrived →
  clear the dim → the `opacity 0.55 → 1` transition fades it in.
  - **Do not** key dim/fade off `Workspace.awaitingResponse`: it is set only by
    the worklist composer / action buttons, so terminal turns never dim. The
    universal "user just spoke" signal is `userText` changing.
  - `heldAssistantText` keeps the prior response visible (dimmed) across the
    turn-start gap, when `/__last-exchange` briefly returns an empty
    `assistantText`.
  - Cite: opacity + transition layout props
    https://docs.xmlui.org/styles-and-themes/layout-props#`opacity` ,
    https://docs.xmlui.org/styles-and-themes/layout-props#`transition` ,
    https://docs.xmlui.org/styles-and-themes/common-units#transition-values ;
    ChangeListener does not fire on initial mount
    https://docs.xmlui.org/howto/debounce-with-changelistener

## Debugging — trace vocabulary

All via `window.__bramIframeTrace` into `resources/bram-traces/bram-trace.log`
(`[iframe] subkind=…`):

| Subkind | Fields | Meaning |
| --- | --- | --- |
| `conversation-refetch` | `reason` | A refresh was triggered (e.g. `talk-session`). |
| `conversation-value-changed` | `toolsLen`, `hasAssistant`, `isRefetch` | The exchange value actually changed. Absence across a poll proves structural sharing held (no needless re-render). |
| `conversation-fade` | `stage` (`dim` \| `fade-in`), `trigger` (on `dim`) | An opacity transition fired. `dim` carries `trigger: 'user-msg'`. |

If the pane flashes again, the tell is `conversation-value-changed` firing on
polls where nothing changed (structural sharing broke), or a section
unmount/remount with no corresponding `conversation-value-changed` (a `when`
gate is keying on something that blanks). Either way, check that the URL is
stable and the gate is `$props.active`, not a `.value`-presence test.

## Out of scope / follow-ups

- The legacy flash-bridge layer (`stickyConversationTools`,
  `liveSubmittedAssistantText`, `__bramComputeConversationSync`) is unused by
  the sections and slated for removal; `awaitingResponse` stays (composer +
  dim use it).
