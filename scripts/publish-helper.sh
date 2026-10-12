#!/usr/bin/env bash
# Publish a windows-bridge.exe build to the helper CDN.
#
# Uploads the binary and rewrites the helper manifest in place, so the next
# client start picks the new sha256 up and re-downloads. Nothing about the
# destination is committed here: host, directory and public URL come from the
# environment.
#
#   export HELPER_PUBLISH_TARGET=user@host
#   export HELPER_PUBLISH_DIR=/path/to/publish/auto/aimux-helpers
#   export HELPER_CDN_BASE=https://cdn.example.com/publish/auto/aimux-helpers
#   scripts/publish-helper.sh --exe out/windows-bridge.exe \
#     --helper-version 0.5.1-aimux5 --aimux-min-version 0.3.70 \
#     --notes "recordStart/recordStop (protocol 5)"
#
# Use --dry-run to see the manifest diff without touching the destination.
set -euo pipefail

EXE=""
PLATFORM="windows-x64"
REMOTE_NAME=""
HELPER_VERSION=""
PROTOCOL_VERSION=""
AIMUX_MIN_VERSION=""
NOTES=""
DRY_RUN=0

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --exe) EXE="${2:-}"; shift 2 ;;
    --platform) PLATFORM="${2:-}"; shift 2 ;;
    --remote-name) REMOTE_NAME="${2:-}"; shift 2 ;;
    --helper-version) HELPER_VERSION="${2:-}"; shift 2 ;;
    --protocol-version) PROTOCOL_VERSION="${2:-}"; shift 2 ;;
    --aimux-min-version) AIMUX_MIN_VERSION="${2:-}"; shift 2 ;;
    --notes) NOTES="${2:-}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$EXE" ]] || die "--exe is required"
[[ -f "$EXE" ]] || die "no such file: $EXE"

TARGET="${HELPER_PUBLISH_TARGET:-}"
DIR="${HELPER_PUBLISH_DIR:-}"
CDN_BASE="${HELPER_CDN_BASE:-}"
[[ -n "$TARGET" && -n "$DIR" && -n "$CDN_BASE" ]] || \
  die "set HELPER_PUBLISH_TARGET, HELPER_PUBLISH_DIR and HELPER_CDN_BASE"

# A wrong-platform artifact behind a Windows-only manifest entry would be
# published as the helper for every client, so check the PE header first.
[[ "$(head -c 2 "$EXE")" == "MZ" ]] || die "$EXE is not a Windows PE executable"

REMOTE_NAME="${REMOTE_NAME:-$(basename "$EXE")}"
SHA="$(sha256sum "$EXE" | cut -d' ' -f1)"
SIZE="$(stat -c %s "$EXE")"
URL="$CDN_BASE/$PLATFORM/$REMOTE_NAME"
MANIFEST_URL="$CDN_BASE/manifest.json"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "[publish] fetching $TARGET:$DIR/manifest.json"
scp -q "$TARGET:$DIR/manifest.json" "$work/manifest.json" \
  || die "could not fetch the manifest; refusing to write a fresh one"

python3 - "$work/manifest.json" <<'PY' "$PLATFORM" "$HELPER_VERSION" "$PROTOCOL_VERSION" "$AIMUX_MIN_VERSION" "$NOTES" "$URL" "$SHA" "$SIZE"
import json, sys

path, platform, helper_version, protocol, aimux_min, notes, url, sha, size = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    manifest = json.load(handle)

entry = (manifest.get("helpers") or {}).get(platform)
if not isinstance(entry, dict):
    sys.exit(f"manifest has no helpers[{platform!r}] entry")

entry["sha256"] = sha
entry["size"] = int(size)
entry["url"] = url
if helper_version:
    entry["helperVersion"] = helper_version
if protocol:
    entry["protocolVersion"] = int(protocol)
if aimux_min:
    entry["aimuxMinVersion"] = aimux_min
if notes:
    entry["notes"] = notes

with open(path, "w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2, ensure_ascii=False)
    handle.write("\n")
print(json.dumps(entry, indent=2, ensure_ascii=False))
PY

if [[ $DRY_RUN -eq 1 ]]; then
  echo "[publish] dry run: manifest not uploaded, binary not uploaded"
  exit 0
fi

echo "[publish] uploading $EXE -> $TARGET:$DIR/$PLATFORM/$REMOTE_NAME"
scp -q "$EXE" "$TARGET:$DIR/$PLATFORM/$REMOTE_NAME"
scp -q "$work/manifest.json" "$TARGET:$DIR/manifest.json"

echo "[publish] verifying $MANIFEST_URL"
served_manifest="$(curl -fsS --max-time 30 "$MANIFEST_URL" 2>/dev/null || true)"
if [[ -z "$served_manifest" ]]; then
  echo "[publish] warning: could not fetch the served manifest, verify by hand" >&2
else
  served="$(printf '%s' "$served_manifest" \
    | python3 -c 'import json,sys; print((json.load(sys.stdin)["helpers"].get(sys.argv[1]) or {}).get("sha256",""))' "$PLATFORM" 2>/dev/null || true)"
  if [[ "$served" == "$SHA" ]]; then
    echo "[publish] ok: served sha256 matches $SHA"
  else
    echo "[publish] warning: served sha256 is '$served' (CDN cache may still hold the old manifest)" >&2
  fi
fi
