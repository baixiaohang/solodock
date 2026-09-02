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
  "attest-package":{"result":"success"},
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
  "attest-package":{"result":"skipped"},
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
  "attest-package":{"result":"skipped"},
  "security-policy":{"result":"success"},
  "docker-e2e":{"result":"skipped"},
  "docker-containerd-e2e":{"result":"skipped"}
}'

"$gate" true true push "$success_needs" >/dev/null
"$gate" true false pull_request "$core_needs" >/dev/null
"$gate" false false pull_request "$docs_needs" >/dev/null

expect_gate_failure() {
  local name=$1
  local run_core=$2
  local run_docker=$3
  local event_name=$4
  local needs=$5
  if "$gate" "$run_core" "$run_docker" "$event_name" "$needs" >/dev/null 2>&1; then
    echo "expected CI gate failure: $name" >&2
    exit 1
  fi
}

expect_gate_failure invalid-classify-output maybe true push "$success_needs"
expect_gate_failure invalid-event true true schedule "$success_needs"
expect_gate_failure unexpected-core-skip true false pull_request "$docs_needs"
expect_gate_failure unexpected-failure true true push "$(jq -c '."rust-test".result = "failure"' <<<"$success_needs")"
expect_gate_failure unexpected-cancellation true true push "$(jq -c '."docker-e2e".result = "cancelled"' <<<"$success_needs")"
expect_gate_failure unexpected-core-run false false pull_request "$success_needs"
expect_gate_failure missing-push-attestation true true push "$(jq -c '."attest-package".result = "skipped"' <<<"$success_needs")"
expect_gate_failure unexpected-pr-attestation true false pull_request "$(jq -c '."attest-package".result = "success"' <<<"$core_needs")"

echo "CI routing and gate tests passed"
