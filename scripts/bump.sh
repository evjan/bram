#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <version>   (e.g. $0 0.1.4)" >&2
  exit 1
fi

VERSION="${1#v}"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version must be N.N.N or vN.N.N (got: $1)" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

sed -i.bak "s/^version = \".*\"/version = \"${VERSION}\"/" src-tauri/Cargo.toml
sed -i.bak "s/\"version\": \"[^\"]*\"/\"version\": \"${VERSION}\"/" src-tauri/tauri.conf.json
rm src-tauri/Cargo.toml.bak src-tauri/tauri.conf.json.bak

echo "Bumped to ${VERSION}:"
grep "^version" src-tauri/Cargo.toml
grep '"version"' src-tauri/tauri.conf.json

echo
echo "Refreshing src-tauri/Cargo.lock (cargo build)..."
cargo build --manifest-path src-tauri/Cargo.toml --quiet

git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json
git commit -m "Release v${VERSION}"
git tag "v${VERSION}"

echo
echo "Tagged v${VERSION} locally. Next:"
echo "  - Push commits (agent-tools 'Push' button or 'git push')"
echo "  - Push the tag separately: git push origin v${VERSION}"
echo "  - Dispatch the Build Binaries workflow with tag v${VERSION}"
