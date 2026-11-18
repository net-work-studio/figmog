#!/usr/bin/env bash
# figmog vertical slice (NWS-2965): synthetic Figma file -> token snapshot ->
# CSS variables + Tailwind v4 @theme -> spec -> card build -> before/after
# evidence at 1440 px and 390 px. Offline apart from npm packages; no Figma
# access, no mirror root, no background owner (every figmog call uses an
# explicit temporary --db and --no-socket).
#
#   examples/slice/run.sh            build everything, write tokens/spec/evidence
#   examples/slice/run.sh --test     ... then run the slice tests (npm test)
#
# Set PLAYWRIGHT_BROWSERS_PATH if the pinned Chromium (Playwright 1.63.0)
# lives outside Playwright's default cache, e.g. /opt/pw-browsers.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

echo "==> cargo build (figmog)"
(cd "$repo" && cargo build --quiet)

echo "==> packages/react: npm ci + build (figmog-shot)"
(cd "$repo/packages/react" && npm ci --no-audit --no-fund --silent && npm run --silent build)

echo "==> examples/slice/app: npm ci"
(cd "$here/app" && npm ci --no-audit --no-fund --silent)

echo "==> slice: tokens -> spec -> builds -> captures -> evidence"
(cd "$here/app" && node scripts/slice.mjs run)

if [[ "${1:-}" == "--test" ]]; then
  echo "==> slice tests"
  (cd "$here/app" && npm test)
fi
