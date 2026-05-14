#!/usr/bin/env bash
set -euo pipefail

# Generate LCOV coverage reports for each library crate in the workspace.
#
# Defaults are intentionally serial and use a unique CARGO_TARGET_DIR per crate to
# avoid cargo-llvm-cov target directory races when multiple agents/worktrees run
# coverage at the same time.
#
# Examples:
#   scripts/coverage-per-crate.sh
#   scripts/coverage-per-crate.sh --crate identity --crate rendezvous
#   scripts/coverage-per-crate.sh --fail-under 90

usage() {
  cat <<'EOF'
Usage: scripts/coverage-per-crate.sh [OPTIONS]

Options:
  --crate NAME        Run one crate by short name (identity) or package name
                      (saorsa-gossip-identity). May be repeated.
  --output-dir DIR    Directory for LCOV and summary output (default: coverage).
  --target-root DIR   Root for per-crate cargo target dirs (default: target).
  --fail-under PCT    Exit non-zero if any measured crate is below PCT.
  --no-clean          Do not run cargo llvm-cov clean before each crate.
  -h, --help          Show this help.

Outputs:
  coverage/<crate>.lcov
  coverage/summary.json
  coverage/summary.md
EOF
}

output_dir="coverage"
target_root="target"
fail_under=""
clean=1
selected=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --crate)
      [[ $# -ge 2 ]] || { echo "error: --crate requires a value" >&2; exit 2; }
      selected+=("$2")
      shift 2
      ;;
    --output-dir)
      [[ $# -ge 2 ]] || { echo "error: --output-dir requires a value" >&2; exit 2; }
      output_dir="$2"
      shift 2
      ;;
    --target-root)
      [[ $# -ge 2 ]] || { echo "error: --target-root requires a value" >&2; exit 2; }
      target_root="$2"
      shift 2
      ;;
    --fail-under)
      [[ $# -ge 2 ]] || { echo "error: --fail-under requires a value" >&2; exit 2; }
      fail_under="$2"
      shift 2
      ;;
    --no-clean)
      clean=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found" >&2
  exit 127
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 not found" >&2
  exit 127
fi

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov is not installed" >&2
  echo "install with: cargo install cargo-llvm-cov" >&2
  exit 127
fi

mkdir -p "$output_dir" "$target_root"

metadata_file="$(mktemp)"
selected_file="$(mktemp)"
crates_file="$(mktemp)"
reports_file="$(mktemp)"
run_id="$$-$(date +%s)"
trap 'rm -f "$metadata_file" "$selected_file" "$crates_file" "$reports_file"' EXIT

cargo metadata --no-deps --format-version 1 > "$metadata_file"
printf '%s\n' "${selected[@]}" > "$selected_file"

python3 - "$metadata_file" "$selected_file" > "$crates_file" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
selected = [line.strip() for line in pathlib.Path(sys.argv[2]).read_text().splitlines() if line.strip()]
selected_set = set(selected)
packages = []
for package in metadata["packages"]:
    has_lib = any("lib" in target.get("kind", []) for target in package.get("targets", []))
    manifest = pathlib.Path(package["manifest_path"])
    if not has_lib or "crates" not in manifest.parts:
        continue
    name = package["name"]
    prefix = "saorsa-gossip-"
    short = name[len(prefix):] if name.startswith(prefix) else name
    if selected_set and name not in selected_set and short not in selected_set:
        continue
    packages.append((short, name))
found = {short for short, _ in packages} | {name for _, name in packages}
missing = sorted(selected_set - found)
if missing:
    print(f"error: unknown crate selection(s): {', '.join(missing)}", file=sys.stderr)
    sys.exit(2)
for short, name in sorted(packages):
    print(f"{short}\t{name}")
PY

crates=()
while IFS= read -r line; do
  [[ -n "$line" ]] && crates+=("$line")
done < "$crates_file"

if [[ ${#crates[@]} -eq 0 ]]; then
  echo "error: no library crates selected" >&2
  exit 2
fi

: > "$reports_file"

echo "Running coverage for ${#crates[@]} crate(s)..."
for row in "${crates[@]}"; do
  IFS=$'\t' read -r short package <<< "$row"
  lcov_path="$output_dir/$short.lcov"
  crate_target_dir="$target_root/llvm-cov-$short-$run_id"

  echo "==> $package ($short)"
  if [[ "$clean" -eq 1 ]]; then
    CARGO_TARGET_DIR="$crate_target_dir" cargo llvm-cov clean --workspace >/dev/null
  fi

  CARGO_TARGET_DIR="$crate_target_dir" \
    cargo llvm-cov \
      --package "$package" \
      --lcov \
      --output-path "$lcov_path"

  printf '%s\t%s\t%s\n' "$short" "$package" "$lcov_path" >> "$reports_file"
done

python3 - "$reports_file" "$output_dir" "$fail_under" <<'PY'
import json
import pathlib
import sys

reports_file = pathlib.Path(sys.argv[1])
output_dir = pathlib.Path(sys.argv[2])
fail_under = float(sys.argv[3]) if sys.argv[3] else None

summary = []
for line in reports_file.read_text().splitlines():
    short, package, lcov_path = line.split("\t")
    lcov = pathlib.Path(lcov_path)
    lf_total = lh_total = 0
    da_found = da_hit = 0
    for raw in lcov.read_text(errors="replace").splitlines():
        if raw.startswith("LF:"):
            try:
                lf_total += int(raw[3:])
            except ValueError:
                pass
        elif raw.startswith("LH:"):
            try:
                lh_total += int(raw[3:])
            except ValueError:
                pass
        elif raw.startswith("DA:"):
            fields = raw[3:].split(",")
            if len(fields) >= 2:
                da_found += 1
                try:
                    count = int(fields[1])
                except ValueError:
                    count = 0
                if count > 0:
                    da_hit += 1
    found = lf_total if lf_total else da_found
    hit = lh_total if lf_total else da_hit
    percent = (hit / found * 100.0) if found else 0.0
    status = "pass" if fail_under is None or percent >= fail_under else "fail"
    summary.append({
        "crate": short,
        "package": package,
        "lcov": str(lcov),
        "lines_found": found,
        "lines_hit": hit,
        "line_coverage": round(percent, 2),
        "status": status,
    })

summary.sort(key=lambda item: item["crate"])
(output_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

lines = [
    "# Per-crate coverage summary",
    "",
    "| Crate | Package | Line coverage | Lines hit/found | Status | LCOV |",
    "|---|---|---:|---:|---|---|",
]
for item in summary:
    lines.append(
        f"| `{item['crate']}` | `{item['package']}` | {item['line_coverage']:.2f}% | "
        f"{item['lines_hit']}/{item['lines_found']} | {item['status']} | `{item['lcov']}` |"
    )
(output_dir / "summary.md").write_text("\n".join(lines) + "\n")

print("\n".join(lines))

if fail_under is not None:
    failed = [item for item in summary if item["line_coverage"] < fail_under]
    if failed:
        print(
            "error: coverage below threshold: "
            + ", ".join(f"{item['crate']}={item['line_coverage']:.2f}%" for item in failed),
            file=sys.stderr,
        )
        sys.exit(1)
PY
