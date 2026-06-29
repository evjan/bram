#!/usr/bin/env python3
# Bram permission-menu surfacing hook (canonical source).
#
# Fires as a Claude Code PreToolUse/PermissionRequest/PostToolUse hook and
# POSTs the structured menu to Bram's loopback so the agent pane can render
# permission menus from data instead of screen-scraping the xterm grid.
#
# - PermissionRequest: a permission dialog is about to show -> POST /__menu/permission
#   with {tool_name, tool_input, permission_suggestions}. (Fires ONLY when a
#   dialog will appear, so it is a false-positive-free "a menu is up" signal.)
# - PostToolUse: the tool ran (i.e. the prompt was answered) -> POST
#   /__menu/permission/clear so a resolved menu doesn't linger.
#
# OBSERVE-ONLY: never returns an allow/deny/ask decision; the user answers in
# the terminal or via the pane (which injects keystrokes). Fully defensive and
# fast: short timeout, fire-and-forget, ALWAYS exits 0 so it can never block or
# delay the prompt.
#
# Installed copy lives at .claude/hooks/permission-menu-hook.py and is refreshed
# from this canonical source by Setup / build.rs. Do not edit the installed copy.
import sys, os, json, urllib.request

PORT_REL = os.path.join("resources", ".bram-port")


def _project_root(payload):
    # Prefer the hook-provided cwd; fall back to CLAUDE_PROJECT_DIR.
    return payload.get("cwd") or os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()


def _port(root):
    try:
        with open(os.path.join(root, PORT_REL)) as f:
            return int(f.read().strip())
    except Exception:
        return None


def _post(port, path, body):
    try:
        data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            "http://127.0.0.1:%d%s" % (port, path),
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        # Short timeout: the hook is synchronous before the prompt renders, so
        # it must not stall. Fire-and-forget; ignore the response.
        urllib.request.urlopen(req, timeout=0.4).read()
    except Exception:
        pass


def main():
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except Exception:
        payload = {}
    event = payload.get("hook_event_name") or ""
    root = _project_root(payload)
    port = _port(root)
    if not port:
        return
    if event == "PermissionRequest":
        _post(port, "/__menu/permission", {
            "tool_name": payload.get("tool_name"),
            "tool_input": payload.get("tool_input") or {},
            "permission_suggestions": payload.get("permission_suggestions") or [],
            "tool_use_id": payload.get("tool_use_id"),
        })
    elif event == "PostToolUse":
        _post(port, "/__menu/permission/clear", {
            "tool_use_id": payload.get("tool_use_id"),
        })


try:
    main()
except Exception:
    pass
sys.exit(0)
