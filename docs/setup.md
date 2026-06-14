# Setup Validation

Bram setup watches two coordination files for staleness:

- `~/.bram/codex-worklist-guard.py`
- `{project}/.claude/bram-conventions.md`

Changing either file's contents should produce one Agent Coordination setup
or refresh banner. Codex has one extra step after setup: the Codex terminal may
ask you to review and approve the hook.

Use harmless content edits for this test. A save that does not change bytes is
not enough.

## Claude First

1. Make a harmless content edit to `~/.bram/codex-worklist-guard.py`.
2. Make a harmless content edit to `{project}/.claude/bram-conventions.md`.
3. Start Claude in the project.
4. Expect one Agent Coordination setup or refresh banner.
5. Click setup or refresh.
6. Expect the completion message to tell you to restart Bram, then start
   Claude or Codex again in the terminal.
7. Repeat the harmless edits to both trigger files.
8. Start Codex in the project.
9. Expect one Agent Coordination setup or refresh banner.
10. Click setup or refresh.
11. Expect Codex to ask for hook review or approval in the terminal on a
    following turn.

## Codex First

1. Make a harmless content edit to `~/.bram/codex-worklist-guard.py`.
2. Make a harmless content edit to `{project}/.claude/bram-conventions.md`.
3. Start Codex in the project.
4. Expect one Agent Coordination setup or refresh banner.
5. Click setup or refresh.
6. Expect Codex to ask for hook review or approval in the terminal on a
   following turn.
7. Expect the completion message to tell you to restart Bram, then start
   Claude or Codex again in the terminal.
8. Start Claude in the project without editing the trigger files again.
9. Expect no Agent Coordination banner, because Codex setup refreshed the
   Claude-side file too.
