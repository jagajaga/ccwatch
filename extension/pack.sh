#!/usr/bin/env bash
# Package the Redline Usage Bridge for both stores.
#
#   extension/pack.sh [OUT_DIR]
#
# Chrome/Chromium: always produces a .zip (upload to the Web Store, or load
# unpacked). Firefox: if AMO_API_KEY and AMO_API_SECRET are set (from your AMO
# account at addons.mozilla.org/developers/addon/api/key/), signs an installable
# .xpi via web-ext for self-distribution. Locally you can source your creds:
#
#   set -a; source ~/Dropbox/Propramming/Firefox/amo-credentials.env; set +a
#   extension/pack.sh
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$DIR/../dist-ext}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"  # absolute — we cd into $DIR before zipping
VER="$(python3 -c "import json;print(json.load(open('$DIR/manifest.json'))['version'])")"

# --- Chrome / Chromium ---
CZIP="$OUT/redline-usage-bridge-chrome-$VER.zip"
rm -f "$CZIP"
( cd "$DIR" && zip -qr -X "$CZIP" . -x '*.DS_Store' -x 'pack.sh' )
echo "chrome  -> $CZIP"

# --- Firefox (signed .xpi, self-distribution) ---
: "${AMO_API_KEY:=${WEB_EXT_API_KEY:-}}"
: "${AMO_API_SECRET:=${WEB_EXT_API_SECRET:-}}"
if [ -n "${AMO_API_KEY:-}" ] && [ -n "${AMO_API_SECRET:-}" ]; then
  npx --yes web-ext@latest sign \
    --source-dir="$DIR" \
    --channel=unlisted \
    --api-key="$AMO_API_KEY" \
    --api-secret="$AMO_API_SECRET" \
    --artifacts-dir="$OUT"
  # Normalize the signed artifact name.
  XPI="$(ls -t "$OUT"/*.xpi 2>/dev/null | head -1 || true)"
  if [ -n "$XPI" ]; then
    mv -f "$XPI" "$OUT/redline-usage-bridge-firefox-$VER.xpi"
    echo "firefox -> $OUT/redline-usage-bridge-firefox-$VER.xpi (signed)"
  fi
else
  echo "firefox -> skipped (set AMO_API_KEY + AMO_API_SECRET to sign the .xpi)"
fi
