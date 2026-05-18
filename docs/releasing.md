# Releasing xmlui-desktop

Debug builds are the shipping format. The Rust side is thin glue (PTY
relay, loopback HTTP file server, small git/sessions queries); the
heavy lifting is XMLUI's TypeScript runtime in the WebView, which is
identical between debug and release. The audience is XMLUI developers,
who benefit from devtools being accessible. Don't propose `cargo build
--release`, code signing, notarization, or installer pipelines.

It's fine to leave `#[cfg(debug_assertions)]` gates in code (e.g.,
`open_devtools`) — they work in the only build we ship.

## Cutting a release

```
scripts/bump.sh 0.1.18
```

This is the atomic release entry point: bumps `src-tauri/Cargo.toml`
+ `src-tauri/tauri.conf.json` to the given version, runs `cargo build`
to refresh `Cargo.lock`, commits as `Release v0.1.18`, creates the
`v0.1.18` tag locally, pushes both commits and tag to `origin`, and
dispatches `.github/workflows/build.yml` against the new tag.

Flags:

- `--no-push` — commit and tag locally only; skip push and workflow
  dispatch. Useful for staging the release commit while you write
  release notes.
- `--branch=<name>` — expected current branch (default `main`). Errors
  out if you're on a different branch.

## Manual fallback

Use when `bump.sh` doesn't fit — re-tagging an existing commit,
dispatching the workflow against an existing tag, etc.

1. Bump `version` in `src-tauri/Cargo.toml` and
   `src-tauri/tauri.conf.json`.
2. `cargo build` to refresh `Cargo.lock`.
3. Commit, then `git tag vX.Y.Z <release-commit>` locally.
4. Push commits via the agent-tools "Push N unpushed commits" button,
   then `git push origin vX.Y.Z` for the tag separately. **The push
   button does not follow tags** — `git ls-remote --tags origin vX.Y.Z`
   after clicking it will return empty until you push the tag
   explicitly.
5. Dispatch `.github/workflows/build.yml` from the GitHub Actions UI
   (or `gh workflow run build.yml -f tag=vX.Y.Z -R judell/xmlui-desktop`)
   with the tag string. The workflow is `workflow_dispatch` only — it
   builds debug binaries for linux-amd64, macos-arm64, macos-intel,
   and windows-amd64, generates SHA256SUMS, and attaches `install.sh`
   / `install.ps1`.

## Testing the update banner

The `/__app-info` route reads the current version from
`CARGO_PKG_VERSION` and compares it against the latest GitHub release.
To exercise the banner UI before actually cutting a new release, launch
with `XMLUI_DESKTOP_FAKE_CURRENT=0.0.1 cargo run` — the env var
substitutes for the real package version in both the comparison and
the response's `current` field, so `has_update` flips to `true`
against whatever the real GitHub latest is, and the banner renders.
The result is cached per process, so set the env var before launch
and restart to re-test with a different fake value.
