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
  cat >"$directory/ci.yml" <<EOF
on: [push]
jobs:
  docker-e2e:
    runs-on: ubuntu-24.04
    services:
      docker:
        image: docker:28.5.2-dind
    steps: []
  attest-package:
    needs: package-smoke
    if: github.event_name == 'push'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-embedded-package
          path: \${{ runner.temp }}/attested-package
      - name: Attest package checksums
        uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS
EOF
  cat >"$directory/extended-ci.yml" <<'EOF'
on: [workflow_dispatch]
jobs:
  docker-resources:
    runs-on: ubuntu-24.04
    services:
      docker:
        image: docker:28.5.2-dind
    steps: []
EOF
  write_safe_release "$directory/release.yml"
  printf '%s\n' "$directory"
}

write_safe_release() {
  local output=$1
  cat >"$output" <<EOF
name: Release
on:
  push:
    tags:
      - 'v[0-9]+.[0-9]+.[0-9]+'
permissions:
  contents: read
concurrency:
  group: release-\${{ github.ref }}
  cancel-in-progress: false
jobs:
  build-release:
    runs-on: ubuntu-24.04
    steps: []
  attest-release:
    needs: build-release
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-release-package
          path: \${{ runner.temp }}/release
      - name: Attest release checksums
        uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/release/SHA256SUMS
  publish-release:
    needs: [build-release, attest-release]
    runs-on: ubuntu-24.04
    permissions:
      contents: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-release-package
          path: \${{ runner.temp }}/release
      - name: Publish release assets
        env:
          GH_TOKEN: \${{ github.token }}
        run: |
          release_dir="\$RUNNER_TEMP/release"
          asset="solodock-\${GITHUB_REF_NAME}-ubuntu-24.04-x86_64.tar.gz"
          gh release create "\$GITHUB_REF_NAME" \\
            "\$release_dir/\$asset" \\
            "\$release_dir/SHA256SUMS" \\
            "\$release_dir/SOURCE_SHA" \\
            --repo "\$GITHUB_REPOSITORY" \\
            --verify-tag \\
            --generate-notes \\
            --title "\$GITHUB_REF_NAME"
EOF
}

fixture_dir_without_ci() {
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
pinned_download='actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c'
pinned_attest='actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6'

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

directory=$(fixture_dir classic-dind-downgrade)
sed -i 's/docker:28.5.2-dind/docker:27-dind/' "$directory/ci.yml"
expect_failure classic-dind-downgrade

directory=$(fixture_dir extended-classic-dind-downgrade)
sed -i 's/docker:28.5.2-dind/docker:28.3.2-dind/' "$directory/extended-ci.yml"
expect_failure extended-classic-dind-downgrade

directory=$(fixture_dir renamed-classic-dind-job)
sed -i 's/^  docker-e2e:/  classic-e2e:/' "$directory/ci.yml"
expect_failure renamed-classic-dind-job

directory=$(fixture_dir renamed-extended-classic-dind-job)
sed -i 's/^  docker-resources:/  classic-resources:/' "$directory/extended-ci.yml"
expect_failure renamed-extended-classic-dind-job

directory=$(fixture_dir missing-extended-classic-dind-workflow)
rm -- "$directory/extended-ci.yml"
expect_failure missing-extended-classic-dind-workflow

directory=$(fixture_dir safe-attestation)
cat >"$directory/ci.yml" <<EOF
name: Safe attestation
on: [push]
permissions:
  contents: read
jobs:
  docker-e2e:
    runs-on: ubuntu-24.04
    services:
      docker:
        image: docker:28.5.2-dind
    steps: []
  attest-package:
    needs: package-smoke
    if: github.event_name == 'push'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-embedded-package
          path: \${{ runner.temp }}/attested-package
      - name: Attest package checksums
        uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS
EOF
"$checker" "$fixture_root/safe-attestation" >/dev/null

directory=$(fixture_dir missing-release-workflow)
rm -- "$directory/release.yml"
expect_failure missing-release-workflow

directory=$(fixture_dir release-pr-trigger)
sed -i 's/^  push:$/  pull_request:/' "$directory/release.yml"
expect_failure release-pr-trigger

directory=$(fixture_dir release-unpinned-attestation)
sed -i "s|$pinned_attest|actions/attest@v4|" "$directory/release.yml"
expect_failure release-unpinned-attestation

directory=$(fixture_dir release-extra-write)
sed -i '/contents: write/a\      actions: write' "$directory/release.yml"
expect_failure release-extra-write

directory=$(fixture_dir release-publish-bypass)
sed -i '/--verify-tag/d' "$directory/release.yml"
expect_failure release-publish-bypass

directory=$(fixture_dir release-forces-latest)
sed -i '/--generate-notes/a\            --latest \\' "$directory/release.yml"
expect_failure release-forces-latest

directory=$(fixture_dir release-attestation-extra-step)
sed -i '/^  attest-release:/,/^  publish-release:/ s/^    steps:$/    steps:\n      - run: env/' "$directory/release.yml"
expect_failure release-attestation-extra-step

directory=$(fixture_dir_without_ci missing-ci-workflow)
cat >"$directory/other.yml" <<'EOF'
on: [push]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure missing-ci-workflow

directory=$(fixture_dir_without_ci renamed-ci-workflow)
cat >"$directory/ci.yaml" <<'EOF'
on: [push]
jobs:
  test:
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure renamed-ci-workflow

directory=$(fixture_dir missing-attestation)
cat >"$directory/ci.yml" <<'EOF'
on: [push]
jobs:
  package-smoke:
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure missing-attestation

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

directory=$(fixture_dir misplaced-attestation-permission)
cat >"$directory/test.yml" <<'EOF'
on: [pull_request]
jobs:
  test:
    permissions:
      id-token: write
    runs-on: ubuntu-24.04
    steps: []
EOF
expect_failure misplaced-attestation-permission

directory=$(fixture_dir attestation-on-pr)
cat >"$directory/ci.yml" <<EOF
on: [pull_request]
jobs:
  attest-package:
    needs: package-smoke
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-embedded-package
          path: \${{ runner.temp }}/attested-package
      - uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS
EOF
expect_failure attestation-on-pr

directory=$(fixture_dir attestation-custom-runner)
cat >"$directory/ci.yml" <<EOF
on: [push]
jobs:
  attest-package:
    needs: package-smoke
    if: github.event_name == 'push'
    runs-on: private-runner
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - uses: $pinned_download
        with:
          name: solodock-embedded-package
          path: \${{ runner.temp }}/attested-package
      - name: Attest package checksums
        uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS
EOF
expect_failure attestation-custom-runner

directory=$(fixture_dir attestation-extra-step)
cat >"$directory/ci.yml" <<EOF
on: [push]
jobs:
  attest-package:
    needs: package-smoke
    if: github.event_name == 'push'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
      id-token: write
      attestations: write
      artifact-metadata: write
    steps:
      - run: env
      - uses: $pinned_download
        with:
          name: solodock-embedded-package
          path: \${{ runner.temp }}/attested-package
      - uses: $pinned_attest
        with:
          subject-path: \${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS
EOF
expect_failure attestation-extra-step

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
