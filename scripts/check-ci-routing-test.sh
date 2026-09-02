#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
classifier="$script_dir/classify-ci-paths.sh"
gate="$script_dir/check-ci-gate.sh"

assert_classification() {
  local expected=$1
  shift
  local actual
  actual=$(printf '%s\n' "$@" | "$classifier")
  if [[ $actual != "$expected" ]]; then
    echo "unexpected classification for: $*" >&2
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
    exit 1
  fi
}

docs_only=$'run_core=false\nrun_docker_e2e=false'
core_only=$'run_core=true\nrun_docker_e2e=false'
core_and_docker=$'run_core=true\nrun_docker_e2e=true'

assert_classification "$docs_only" README.md docs/testing.md
assert_classification "$core_only" web/src/App.svelte
assert_classification "$core_and_docker" src/presets/postgres.rs
assert_classification "$core_and_docker" src/settings.rs
assert_classification "$core_and_docker" future/unknown-file.txt
assert_classification "$core_and_docker" ''

success_needs='{
  "classify":{"result":"success"},
  "web":{"result":"success"},
  "rust-lint":{"result":"success"},
  "rust-test":{"result":"success"},
  "package-smoke":{"result":"success"},
  "security-policy":{"result":"success"},
  "docker-e2e":{"result":"success"},
  "docker-containerd-e2e":{"result":"success"}
}'
docs_needs='{
  "classify":{"result":"success"},
  "web":{"result":"skipped"},
  "rust-lint":{"result":"skipped"},
  "rust-test":{"result":"skipped"},
  "package-smoke":{"result":"skipped"},
  "security-policy":{"result":"success"},
  "docker-e2e":{"result":"skipped"},
  "docker-containerd-e2e":{"result":"skipped"}
}'
core_needs='{
  "classify":{"result":"success"},
  "web":{"result":"success"},
  "rust-lint":{"result":"success"},
  "rust-test":{"result":"success"},
  "package-smoke":{"result":"success"},
  "security-policy":{"result":"success"},
  "docker-e2e":{"result":"skipped"},
  "docker-containerd-e2e":{"result":"skipped"}
}'

"$gate" true true "$success_needs" >/dev/null
"$gate" true false "$core_needs" >/dev/null
"$gate" false false "$docs_needs" >/dev/null

expect_gate_failure() {
  local name=$1
  local run_core=$2
  local run_docker=$3
  local needs=$4
  if "$gate" "$run_core" "$run_docker" "$needs" >/dev/null 2>&1; then
    echo "expected CI gate failure: $name" >&2
    exit 1
  fi
}

expect_gate_failure invalid-classify-output maybe true "$success_needs"
expect_gate_failure unexpected-core-skip true false "$docs_needs"
expect_gate_failure unexpected-failure true true "$(jq -c '."rust-test".result = "failure"' <<<"$success_needs")"
expect_gate_failure unexpected-cancellation true true "$(jq -c '."docker-e2e".result = "cancelled"' <<<"$success_needs")"
expect_gate_failure unexpected-core-run false false "$success_needs"

echo "CI routing and gate tests passed"
