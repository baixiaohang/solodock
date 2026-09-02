#!/usr/bin/env bash
set -euo pipefail

run_core=false
run_docker_e2e=false
saw_path=false

while IFS= read -r path; do
  [[ -z $path ]] && continue
  saw_path=true

  case "$path" in
    *.md | LICENSE | NOTICE)
      ;;
    *)
      run_core=true
      ;;
  esac

  case "$path" in
    *.md | LICENSE | NOTICE | web/* | .github/dependabot.yml | .github/workflows/codeql.yml | deny.toml | scripts/check-ci-*.sh | scripts/check-workflow-security*)
      ;;
    *)
      # Docker/部署影响必须 fail closed：只有上面的明确安全路径可以跳过 DinD。
      run_docker_e2e=true
      ;;
  esac
done

if [[ $saw_path == false ]]; then
  # 空或无法识别的变更集合不能被当作 docs-only。
  run_core=true
  run_docker_e2e=true
fi

printf 'run_core=%s\n' "$run_core"
printf 'run_docker_e2e=%s\n' "$run_docker_e2e"
