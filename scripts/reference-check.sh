#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
run=".codex/runs/reference-$(date -u +%Y%m%dT%H%M%S)-$$"
mkdir -p "$run"
exec > >(tee "$run/output.log") 2>&1
printf 'Run: %s\n' "$run"
rustc --version
cargo --version
git rev-parse HEAD
git diff --binary > "$run/workspace.patch"
git status --short
find reference tests -type f -print0 | sort -z | xargs -0 sha256sum > "$run/source-sha256.txt"
python3 - <<'CHECK'
import hashlib
import json
from pathlib import Path
root = Path('reference/cc-switch-reference')
manifest = json.loads((root / 'provenance.json').read_text())
for entry in manifest['files']:
    digest = hashlib.sha256((root / 'src' / entry['target']).read_bytes()).hexdigest()
    if digest != entry['sha256']:
        raise SystemExit('Reference source changed: ' + entry['target'])
print('Pinned reference source hashes verified:', manifest['commit'])
CHECK
cargo test --test differential "$@"
