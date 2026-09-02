#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
checker="$script_dir/check-workflow-security.sh"
fixture_root=$(mktemp -d)
trap 'rm -rf "$fixture_root"' EXIT

fixture_dir() {
  local name=$1
  local directory="$fixture_root/$name/.github/workflows"
  mkdir -p "$directory"
  printf '%s\n' "$directory"
}

expect_failure() {
  local name=$1
  if "$checker" "$fixture_root/$name" >/dev/null 2>&1; then
    echo "expected workflow policy failure: $name" >&2
    exit 1
  fi
}

pinned_checkout='actions/checkout@11d5960a326750d5838078e36cf38b85af677262'
pinned_codeql='github/codeql-action/analyze@cdf488f595d80d6e07e03d4674febd5ab45fa938'

directory=$(fixture_dir safe)
cat >"$directory/test.yml" <<EOF
name: Safe
on: [pull_request]
permissions:
  contents: read
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - uses: $pinned_checkout
        with:
          persist-credentials: false
EOF
"$checker" "$fixture_root/safe" >/dev/null

directory=$(fixture_dir safe-codeql)
cat >"$directory/codeql.yml" <<EOF
name: Safe CodeQL
on: [pull_request]
permissions:
  contents: read
jobs:
  analyze:
    permissions:
      contents: read
      security-events: write
    runs-on: ubuntu-24.04
    steps:
      - uses: $pinned_codeql
EOF
"$checker" "$fixture_root/safe-codeql" >/dev/null

directory=$(fixture_dir unpinned)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
EOF
expect_failure unpinned

directory=$(fixture_dir dangerous-trigger)
cat >"$directory/test.yml" <<EOF
on: [pull_request_target]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - uses: $pinned_checkout
        with:
          persist-credentials: false
EOF
expect_failure dangerous-trigger

directory=$(fixture_dir self-hosted-array)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    runs-on:
      - self-hosted
      - linux
    steps: []
EOF
expect_failure self-hosted-array

directory=$(fixture_dir secret-reference)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - run: echo "${{ secrets['EXAMPLE'] }}"
EOF
expect_failure secret-reference

directory=$(fixture_dir quoted-write)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
permissions:
  contents: "write"
jobs:
  test:
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure quoted-write

directory=$(fixture_dir write-all)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
permissions: "write-all"
jobs:
  test:
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure write-all

directory=$(fixture_dir checkout-padding)
cat >"$directory/test.yml" <<EOF
on: [pull_request]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - uses: $pinned_checkout
      - uses: $pinned_codeql
        with:
          persist-credentials: false
EOF
expect_failure checkout-padding

directory=$(fixture_dir local-action)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - uses: ./.github/actions/example
EOF
expect_failure local-action

directory=$(fixture_dir misplaced-security-events)
cat >"$directory/codeql.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    permissions:
      security-events: write
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure misplaced-security-events

directory=$(fixture_dir continue-on-error)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps:
      - run: false
        continue-on-error: true
EOF
expect_failure continue-on-error

echo "workflow security policy tests passed"
