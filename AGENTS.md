# Bram

You are running in the left pane of a Tauri desktop shell that puts a real terminal next to an XMLUI surface. The user can see the right pane while talking to you, so use it.

## Right Pane

When the user asks for something that benefits from structured output or structured input, edit `Main.xmlui` or files under `components/` so the right pane renders it. A filesystem watcher reloads the iframe automatically when you save, so you do not need to ask the user to refresh.

Examples:

- Show tables, lists, charts, or other structured results in the right pane.
- Use selectors, forms, step flows, or other structured input when the user needs to choose or confirm something.
- Prefer XMLUI-native interaction patterns instead of pushing everything through chat.

## Working In XMLUI Surfaces

Both panes here are XMLUI, and most edits land in `.xmlui` files.

- Avoid raw browser JS in event handlers.
- Prefer XMLUI-native abstractions such as `delay`, `debounce`, `Timer`, `DataSource`, and `ChangeListener`.
- Read `app/__shell/conventions.md` for the authoritative Bram-specific workflow, including the worklist lifecycle and approval flow.
- When a markup choice is non-obvious, cite the XMLUI docs URL for the component or howto you are using.

## Worklist Coordination

`resources/worklist.json` is the canonical surface for coordinating multi-step work between you and the user. Use it whenever you would otherwise enumerate a small set of independently approvable changes in prose.

The full proposed -> applied -> committed flow, authorization payloads, mutate/resolve behavior, and edge cases live in `app/__shell/conventions.md`. Do not duplicate that whole policy here; treat it as the source of truth.

## Files You Will Edit Most

- `Main.xmlui` - the main XMLUI surface.
- `components/*.xmlui` - workspace panels and supporting UI.
- `config.json` - XMLUI app configuration.
- `resources/*.svg` - custom icons registered in `config.json`.
- `app/__shell/helpers.js` - window helpers loaded by `index.html`.

## Files To Leave Alone Unless Asked

- `src-tauri/src/lib.rs` - Rust backend.
- `app/main.js` and `app/index.html` - parent shell wiring.
- `app/vendor/*` - vendored libraries.

## Inspector And Debugging

The right pane mounts an Inspector in the AppHeader profile menu slot. Use it when you are debugging interactions before assuming the markup is wrong.

When a UI issue needs deeper inspection, ask the user to reproduce it with the Inspector open and export a trace, then analyze the trace instead of guessing from the markup.

## Architectural Background

The deeper background for Bram's shell architecture, runtime behavior, and gotchas lives in `~/.agents/scout/projects/claude-code-desktop.md`. Read it if a mechanism here surprises you.

<!-- xmlui-desktop:start -->
@app/__shell/conventions.md
<!-- xmlui-desktop:end -->
