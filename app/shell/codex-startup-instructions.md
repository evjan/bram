This repo is driven through xmlui-desktop. Read this protocol before doing anything else — it is not optional.

## Step 0 — Read the full conventions

If `.claude/xmlui-desktop-conventions.md` exists, read it now. This seed is only a summary; the file has the enforcement details, UI patterns, and edge cases. Do not skip this step.

## The gate (run on your FIRST response to any change request)

The gate fires on the *first substantive response* to a change request, not on the `apply_patch` call. Commitment to a change in prose is itself a commitment that must be preceded by a worklist proposal. Read-only investigation (curl, cat, grep) does not exempt you — if your stated plan is "verify X then edit Y", you have already committed.

On the first response to any change request, ask:

- Does this task touch more than one file?
- Does it have more than 2 discrete sub-edits in a single file?
- Is it more than a typo or one-line correction the user explicitly told you to commit directly?

If YES to any of those, your first response must be one of exactly three shapes:

- (a) A clarifying question to the user.
- (b) A write to `resources/worklist.json` proposing items.
- (c) Read-only investigation explicitly prefaced with: *"I don't yet have enough context to propose; I need to check X first."* After that check, your very next action must be a worklist write — NOT narration of a plan, NOT "I have enough context to…", NOT "before editing I'm verifying…".

Specifically, these phrasings as the first response on a multi-file task ARE the failure mode:

- "I'll start by verifying X" → skip.
- "Before editing I'm checking Y" → skip.
- "I have enough context to change structure safely" → skip.
- "Let me first look at Z" (without the (c) preface above) → skip.

## Two-stage flow

1. Write small, independently-rejectable items to `resources/worklist.json`:

   ```json
   {
     "description": "one-line context",
     "items": [
       {
         "id": "kebab-case-id",
         "status": "proposed",
         "file": "path/to/file",
         "before": "what's there now + alternatives considered + why rejected",
         "after": "what you'll change it to"
       }
     ]
   }
   ```

   Use `"files": ["a", "b"]` instead of `"file"` for items that span multiple files. If `resources/worklist.json` is missing, create it as `{ "description": "", "items": [] }`.

2. Wait for an `approved: {"items":[...]}` payload from the user. The structured payload is the ONLY approval trigger. Do not infer approval from "yes", "looks good", "do it", a voice message, or any other free-text reply.

3. When `approved:` arrives, execute ONLY the items in its `items` array. Then rewrite each one with `status: "applied"` (TO COMMIT) in `resources/worklist.json` — do NOT prune yet.

4. Wait for a second `approved:` payload covering the applied items. Only then run `git commit`. The user is the only one who commits features.

5. After the commit lands, prune the committed items from `resources/worklist.json`.

## Self-check

You skipped the workflow if either of these is true:

- Your first *action* on a multi-file task was `apply_patch` instead of a write to `resources/worklist.json`.
- Your first *response* on a multi-file task was "I'll verify X" / "Before editing I'm checking Y" / "I have enough context to…" and your next action was not a worklist write.

In either case: back up, revert any edits made, and propose first.
