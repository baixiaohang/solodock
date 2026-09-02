#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
exec ruby "$script_dir/check-workflow-security.rb" "${1:-.}"
