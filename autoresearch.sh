#!/usr/bin/env bash
set -euo pipefail

# Run coverage for the rendezvous crate and emit METRIC lines.
# Uses a unique temp target root to avoid races with other worktrees.

RUN_ID="$$-$(date +%s)"
OUTPUT_DIR="coverage-$$"
TARGET_ROOT="target-$$"

cleanup() {
  rm -rf "$OUTPUT_DIR" "$TARGET_ROOT"
}
trap cleanup EXIT

scripts/coverage-per-crate.sh \
  --crate rendezvous \
  --output-dir "$OUTPUT_DIR" \
  --target-root "$TARGET_ROOT" >/dev/null 2>&1

python3 - "$OUTPUT_DIR/summary.json" <<'PY'
import json, sys
from pathlib import Path

summary = json.loads(Path(sys.argv[1]).read_text())
item = summary[0]
print(f"METRIC coverage_pct={item['line_coverage']}")
print(f"METRIC lines_hit={item['lines_hit']}")
print(f"METRIC lines_found={item['lines_found']}")
uncovered = item['lines_found'] - item['lines_hit']
print(f"METRIC uncovered_lines={uncovered}")
PY
