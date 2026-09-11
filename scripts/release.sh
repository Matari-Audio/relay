#!/usr/bin/env bash
# RELAY release pipeline. Runs on your machine, not CI: apps/plugin builds
# against the sibling truce checkout, which CI does not have.
#
#   scripts/release.sh              gate, then package this host's installers
#   scripts/release.sh build [...]  package only; extra args go to cargo truce
#   scripts/release.sh gate         the gate only
#   scripts/release.sh publish v0.1.0
#
# One host builds one platform. Run it on each OS you ship and drop the
# resulting files into the same dist/ before publishing.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
dist="$root/dist"
plugin_manifest="$root/apps/plugin/Cargo.toml"

say() { printf '\n==> %s\n' "$*"; }

meta() {
  cargo metadata --no-deps --format-version 1 --manifest-path "$plugin_manifest" | jq -r "$1"
}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

gate() {
  say "workspace: format, lints, tests"
  cargo fmt --all -- --check
  cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
  cargo test --locked --workspace --all-targets

  say "plugin: format, lints, tests"
  cd apps/plugin
  cargo fmt -- --check
  cargo clippy --all-targets -- -D warnings
  cargo test --all-targets
  cd "$root"

  # The site is not in this artifact — it deploys on its own (`just
  # link-deploy`) and has its own checks (`just web-test`). Keeping pnpm out
  # of the release path means a cold node_modules cannot block a build.
}

build() {
  local version target out
  version="$(meta '.packages[] | select(.name == "relay-plugin") | .version')"
  target="$(meta .target_directory)"
  out="$target/dist"

  say "packaging RELAY $version for $(uname -s)"
  # Signing and notarization are on by default; a machine without the
  # credentials should pass --no-notarize / --no-sign here.
  ( cd apps/plugin && cargo truce package "$@" )

  mkdir -p "$dist"
  shopt -s nullglob
  local made=("$out"/*)
  if [ ${#made[@]} -eq 0 ]; then
    echo "cargo truce produced nothing in $out" >&2
    exit 1
  fi
  cp -f "${made[@]}" "$dist/"

  # Regenerated over everything staged, so a machine that collects the other
  # platforms' files signs for all of them.
  ( cd "$dist" && rm -f SHA256SUMS.txt && sha256 -- * > SHA256SUMS.txt )
  say "staged in dist/"
  ls -lh "$dist"
}

publish() {
  local tag="${1:-}"
  [ -n "$tag" ] || { echo "usage: scripts/release.sh publish <tag>" >&2; exit 1; }
  [ -d "$dist" ] || { echo "nothing in dist/ — run scripts/release.sh first" >&2; exit 1; }
  if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "working tree is dirty; a published build must match a commit" >&2
    exit 1
  fi
  say "publishing $tag"
  if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$dist"/* --clobber
  else
    gh release create "$tag" "$dist"/* --title "RELAY $tag" --generate-notes
  fi
}

case "${1:-all}" in
  gate) gate ;;
  build) shift; build "$@" ;;
  publish) shift; publish "$@" ;;
  all) gate; build ;;
  *) sed -n '2,12p' "$0" >&2; exit 1 ;;
esac
