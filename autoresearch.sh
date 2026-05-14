#!/usr/bin/env bash
set -euo pipefail

scripts/coverage-per-crate.sh --crate runtime
python3 - <<'PY'
import json
from pathlib import Path
summary = json.loads(Path('coverage/summary.json').read_text())
runtime = next(item for item in summary if item['crate'] == 'runtime')
print(f"METRIC line_coverage={runtime['line_coverage']}")
print(f"METRIC lines_hit={runtime['lines_hit']}")
print(f"METRIC lines_found={runtime['lines_found']}")
PY
