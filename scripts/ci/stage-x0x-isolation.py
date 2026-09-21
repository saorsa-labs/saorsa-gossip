#!/usr/bin/env python3
"""Stage the four hash-pinned x0x isolation helpers into scripts/ci (#504).

saorsa-gossip does not vendor the x0x isolation harness. This helper retrieves
exactly four files from one immutable upstream commit over the disposable CI
network, verifies every SHA-256 while the copies are still outside the
workspace, and only then writes them beside this script under scripts/ci/.
A provenance manifest (upstream commit + per-file hashes) is retained under
RUNNER_TEMP, separate from SG source custody.

Fail-closed guarantees:

* retrieval failure or HTTP status != 200  -> nonzero exit, nothing staged;
* any SHA-256 mismatch                     -> nonzero exit before the first
  byte enters the workspace, temporary copies destroyed;
* an existing different file at a staging
  destination                              -> nonzero exit, existing file
  left untouched (never overwritten);
* dirty tracked tree before staging, or a
  worktree that is not exactly the four
  staged helpers plus the generated
  Cargo.lock afterwards (--verify-tree)    -> nonzero exit.

No mutable ref is followed and nothing is cloned: the retrieval URLs pin the
commit. This script runs only in the disposable Ubuntu CI job.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request

X0X_COMMIT = "c481f62d7124e92ff3b14414cce88ca35aa18c66"
X0X_RAW_BASE = (
    "https://raw.githubusercontent.com/saorsa-labs/x0x/"
    + X0X_COMMIT
    + "/scripts/ci"
)
HELPERS = {
    "nextest-isolated.sh":
        "4c09d3f5fc21e12045f03acabb00d2a38f243d5566cef83392d9e78d83a99fc1",
    "nextest-reuse.py":
        "33a86e237d5ae5fb6d3ce694ccc5d92df08508068649216dd79a9048eb14596f",
    "isolated-runtime.py":
        "34d071fa13283089074a7c7c4186915d8d99738d1a72fd40557319b8b81dbfe2",
    "isolation-custody-collect.py":
        "696033b398611f95eaaeaa9b0aa71553320a463f8ee238966c910befbd29aac0",
}
MANIFEST_SCHEMA = "sg.504-x0x-harness-staging/1"
PROVENANCE_DIR = "sg-504-harness-provenance"


def die(message):
    print(f"stage-x0x-isolation: {message}", file=sys.stderr)
    raise SystemExit(1)


def sha256_file(path):
    with open(path, "rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def git_porcelain(root):
    result = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain", "-uall"],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        die(f"git status failed: {result.stderr.strip()}")
    return [line for line in result.stdout.splitlines() if line]


def fetch(url, destination):
    request = urllib.request.Request(url, headers={"User-Agent": "sg-ci-harness-staging"})
    try:
        with urllib.request.urlopen(request, timeout=90) as response:
            if response.status != 200:
                die(f"retrieval {url} returned HTTP {response.status}")
            destination.write_bytes(response.read())
    except Exception as error:  # any retrieval failure is reported and fatal
        die(f"retrieval {url} failed: {error}")


def write_manifest(runner_temp):
    out_dir = runner_temp / PROVENANCE_DIR
    try:
        out_dir.mkdir()
    except (FileExistsError, NotADirectoryError) as error:
        die(f"provenance output path is not exclusively creatable: {out_dir}")
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "x0x_commit": X0X_COMMIT,
        "x0x_raw_base": X0X_RAW_BASE,
        "github_sha": os.environ.get("GITHUB_SHA", ""),
        "files": [
            {
                "path": f"scripts/ci/{name}",
                "sha256": expected,
                "bytes": (Path.cwd() / "scripts" / "ci" / name).stat().st_size,
            }
            for name, expected in sorted(HELPERS.items())
        ],
    }
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    handle = os.open(out_dir / "manifest.json", flags, 0o644)
    with os.fdopen(handle, "w", encoding="utf-8") as stream:
        stream.write(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"manifest": str(out_dir / "manifest.json"), "files": len(HELPERS)}))


def stage(runner_temp):
    root = Path.cwd()
    scripts_dir = root / "scripts"
    if not scripts_dir.is_dir():
        die("scripts/ does not exist; helpers must stage beside existing sibling paths")
    dirty = git_porcelain(root)
    if dirty:
        die("worktree is dirty before staging:\n  " + "\n  ".join(dirty))

    scratch = Path(tempfile.mkdtemp(prefix="x0x-harness-staging-", dir=runner_temp))
    try:
        downloads = {}
        for name, expected in HELPERS.items():
            target = scratch / name
            fetch(f"{X0X_RAW_BASE}/{name}", target)
            digest = sha256_file(target)
            if digest != expected:
                die(f"hash mismatch for {name}: got {digest}, expected {expected}")
            downloads[name] = target
        # Every hash is verified before the first byte enters the workspace.
        dest_dir = scripts_dir / "ci"
        dest_dir.mkdir(exist_ok=True)
        for name, source in downloads.items():
            dest = dest_dir / name
            if dest.exists():
                if sha256_file(dest) != HELPERS[name]:
                    die(f"refusing to overwrite existing different file: {dest}")
                continue
            mode = 0o755 if name.endswith(".sh") else 0o644
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
            handle = os.open(dest, flags, mode)
            with os.fdopen(handle, "wb") as out:
                out.write(source.read_bytes())
        for name, expected in HELPERS.items():
            if sha256_file(dest_dir / name) != expected:
                die(f"post-staging hash mismatch for scripts/ci/{name}")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    write_manifest(runner_temp)


def verify_tree():
    root = Path.cwd()
    expected = sorted(f"?? scripts/ci/{name}" for name in HELPERS)
    observed = git_porcelain(root)
    if observed != expected:
        die(
            "unexpected worktree state: want exactly the four staged helpers as "
            "the only untracked files (generated Cargo.lock and target/ are "
            "gitignored), observed:\n  " + "\n  ".join(observed)
        )
    for name, wanted in HELPERS.items():
        digest = sha256_file(root / "scripts" / "ci" / name)
        if digest != wanted:
            die(f"staged helper changed on disk: scripts/ci/{name} ({digest})")
    if not (root / "Cargo.lock").is_file():
        die("generated Cargo.lock is missing before the isolated run")
    print(json.dumps({"tree": "clean", "untracked": expected, "lock": "present"}))


def main():
    runner_temp = os.environ.get("RUNNER_TEMP")
    if not runner_temp:
        die("RUNNER_TEMP is required")
    if len(sys.argv) == 1:
        stage(Path(runner_temp))
    elif sys.argv[1:] == ["--verify-tree"]:
        verify_tree()
    else:
        die(f"usage: {sys.argv[0]} [--verify-tree]")


if __name__ == "__main__":
    main()
