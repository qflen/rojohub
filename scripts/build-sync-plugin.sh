#!/usr/bin/env bash
# Build RojoHub's Studio sync plugin (plugin/RojoHubSync.rbxm).
#
# The plugin is a fork of the Rojo Studio plugin (MPL-2.0). The repo stores
# only an overlay of changed/added files under sync-plugin/overlay; this
# script fetches upstream Rojo at the pinned tag, lays the overlay over it
# and runs `rojo build` on the result. Upstream is treated as untrusted
# input: nothing from the clone is executed except through `rojo build`.
#
# Environment:
#   ROJO_BIN   path to a rojo binary (default: `rojo` from PATH)
#   LUNE_BIN   path to a lune binary (default: `lune` from PATH; tests are skipped if absent)
#   ROJOHUB_UPSTREAM_CACHE  directory for the cached clone (default: <repo>/.cache/rojo-upstream-<tag>)
set -euo pipefail

UPSTREAM_REPO="https://github.com/rojo-rbx/rojo.git"
UPSTREAM_TAG="v7.7.1"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OVERLAY_DIR="$REPO_ROOT/sync-plugin/overlay"
OUTPUT_REL="plugin/RojoHubSync.rbxm"
OUTPUT="$REPO_ROOT/$OUTPUT_REL"
CACHE_DIR="${ROJOHUB_UPSTREAM_CACHE:-$REPO_ROOT/.cache/rojo-upstream-$UPSTREAM_TAG}"

ROJO_BIN="${ROJO_BIN:-rojo}"
LUNE_BIN="${LUNE_BIN:-}"

log() { printf '[build-sync-plugin] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

command -v "$ROJO_BIN" >/dev/null 2>&1 || [ -x "$ROJO_BIN" ] || die "rojo not found (set ROJO_BIN or put rojo on PATH)"
command -v git >/dev/null 2>&1 || die "git is required"
[ -d "$OVERLAY_DIR" ] || die "overlay directory missing: $OVERLAY_DIR"

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

# --- 1. Fetch or reuse upstream at the pinned tag ---------------------------
if [ -d "$CACHE_DIR/.git" ] && [ "$(git -C "$CACHE_DIR" describe --tags --exact-match 2>/dev/null || true)" = "$UPSTREAM_TAG" ]; then
	log "reusing cached upstream clone at $CACHE_DIR"
	# Drop whatever a previous build left behind so the overlay is applied to pristine upstream.
	git -C "$CACHE_DIR" checkout --quiet -- .
	git -C "$CACHE_DIR" clean --quiet -fd
	git -C "$CACHE_DIR" submodule --quiet update --init --recursive --depth 1
else
	log "cloning $UPSTREAM_REPO at $UPSTREAM_TAG into $CACHE_DIR"
	rm -rf "$CACHE_DIR"
	mkdir -p "$(dirname "$CACHE_DIR")"
	git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$UPSTREAM_TAG" --recurse-submodules --shallow-submodules "$UPSTREAM_REPO" "$CACHE_DIR"
fi

actual_tag="$(git -C "$CACHE_DIR" describe --tags --exact-match 2>/dev/null || true)"
[ "$actual_tag" = "$UPSTREAM_TAG" ] || die "upstream clone is at '$actual_tag', expected $UPSTREAM_TAG"
log "upstream commit: $(git -C "$CACHE_DIR" rev-parse HEAD)"

# --- 2. Apply the overlay ---------------------------------------------------
log "applying overlay from $OVERLAY_DIR"
(cd "$OVERLAY_DIR" && find . -type f -print0) | while IFS= read -r -d '' rel; do
	rel="${rel#./}"
	mkdir -p "$CACHE_DIR/$(dirname "$rel")"
	cp "$OVERLAY_DIR/$rel" "$CACHE_DIR/$rel"
	log "  $rel"
done

# --- 3. Build -----------------------------------------------------------------
mkdir -p "$(dirname "$OUTPUT")"
log "building with $("$ROJO_BIN" --version)"
(cd "$CACHE_DIR" && "$ROJO_BIN" build plugin.project.json -o "$OUTPUT")
[ -s "$OUTPUT" ] || die "build produced no output at $OUTPUT"

log "wrote $OUTPUT_REL ($(wc -c <"$OUTPUT" | tr -d ' ') bytes)"
printf 'sha256  %s  %s\n' "$(sha256_of "$OUTPUT")" "$OUTPUT_REL"

# --- 4. Policy unit tests (lune) ----------------------------------------------
if [ -z "$LUNE_BIN" ] && command -v lune >/dev/null 2>&1; then
	LUNE_BIN="lune"
fi
if [ -n "$LUNE_BIN" ] && { command -v "$LUNE_BIN" >/dev/null 2>&1 || [ -x "$LUNE_BIN" ]; }; then
	log "running policy tests with $("$LUNE_BIN" --version)"
	(cd "$REPO_ROOT" && "$LUNE_BIN" run sync-plugin/tests/policy.luau)
else
	log "lune not found; skipping policy tests (set LUNE_BIN or put lune on PATH)"
fi
