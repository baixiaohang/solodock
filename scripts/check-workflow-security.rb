#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

PINNED_ACTION = %r{\A[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*@[0-9a-f]{40}\z}
DANGEROUS_TRIGGERS = %w[pull_request_target workflow_run].freeze
ATTEST_JOB = "attest-package"
RELEASE_ATTEST_JOB = "attest-release"
RELEASE_PUBLISH_JOB = "publish-release"
DOWNLOAD_ARTIFACT_ACTION = "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c"
ATTEST_ACTION = "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6"
ATTEST_PERMISSIONS = {
  "contents" => "read",
  "id-token" => "write",
  "attestations" => "write",
  "artifact-metadata" => "write"
}.freeze

root = File.expand_path(ARGV.fetch(0, "."))
workflow_dir = File.join(root, ".github", "workflows")
abort "workflow directory not found: #{workflow_dir}" unless Dir.exist?(workflow_dir)

workflow_files = Dir.glob(File.join(workflow_dir, "*.{yml,yaml}")).sort
abort "no workflow files found in #{workflow_dir}" if workflow_files.empty?

errors = []
required_ci_file = File.join(workflow_dir, "ci.yml")
unless File.file?(required_ci_file) && !File.symlink?(required_ci_file)
  errors << "#{required_ci_file}: required workflow must exist as a regular file"
end
required_release_file = File.join(workflow_dir, "release.yml")
unless File.file?(required_release_file) && !File.symlink?(required_release_file)
  errors << "#{required_release_file}: required workflow must exist as a regular file"
end

scalar_strings = lambda do |value, &block|
  case value
  when Hash
    value.each do |key, child|
      block.call(key.to_s)
      scalar_strings.call(child, &block)
    end
  when Array
    value.each { |child| scalar_strings.call(child, &block) }
  when String
    block.call(value)
  end
end

permission_errors = lambda do |permissions, file, scope|
  if permissions.is_a?(String)
    if permissions == "write-all"
      errors << "#{file}: #{scope} permissions must not use write-all"
    elsif permissions != "read-all"
      errors << "#{file}: unsupported #{scope} permissions value: #{permissions.inspect}"
    end
    next
  end
  unless permissions.nil? || permissions.is_a?(Hash)
    errors << "#{file}: #{scope} permissions must be a mapping, read-all, or write-all"
    next
  end

  permissions&.each do |name, access|
    next unless access.to_s == "write"

    allowed_codeql_upload =
      File.basename(file) == "codeql.yml" && scope == "job analyze" && name.to_s == "security-events"
    allowed_package_attestation =
      File.basename(file) == "ci.yml" && scope == "job #{ATTEST_JOB}" &&
      %w[id-token attestations artifact-metadata].include?(name.to_s)
    allowed_release_attestation =
      File.basename(file) == "release.yml" && scope == "job #{RELEASE_ATTEST_JOB}" &&
      %w[id-token attestations artifact-metadata].include?(name.to_s)
    allowed_release_publish =
      File.basename(file) == "release.yml" && scope == "job #{RELEASE_PUBLISH_JOB}" &&
      name.to_s == "contents"
    unless allowed_codeql_upload || allowed_package_attestation || allowed_release_attestation || allowed_release_publish
      errors << "#{file}: unapproved write permission in #{scope}: #{name}"
    end
  end
end

workflow_files.each do |file|
  begin
    document = YAML.safe_load(File.read(file), aliases: false)
  rescue Psych::Exception => e
    errors << "#{file}: invalid or unsupported YAML: #{e.message}"
    next
  end
  unless document.is_a?(Hash)
    errors << "#{file}: workflow root must be a mapping"
    next
  end

  trigger_node = document.key?("on") ? document["on"] : document[true]
  triggers = case trigger_node
             when String then [trigger_node]
             when Array then trigger_node.map(&:to_s)
             when Hash then trigger_node.keys.map(&:to_s)
             else
               errors << "#{file}: workflow trigger must be a string, sequence, or mapping"
               []
             end
  DANGEROUS_TRIGGERS.each do |trigger|
    errors << "#{file}: forbidden trigger: #{trigger}" if triggers.include?(trigger)
  end

  if File.basename(file) == "release.yml"
    expected_trigger = { "push" => { "tags" => ["v[0-9]+.[0-9]+.[0-9]+"] } }
    errors << "#{file}: release workflow must use only the fixed version-tag trigger" unless trigger_node == expected_trigger
    errors << "#{file}: release workflow must default to contents: read" unless document["permissions"] == { "contents" => "read" }
    expected_concurrency = { "group" => "release-${{ github.ref }}", "cancel-in-progress" => false }
    errors << "#{file}: release workflow must use non-cancelling per-ref concurrency" unless document["concurrency"] == expected_concurrency
  end

  permission_errors.call(document["permissions"], file, "workflow")

  scalar_strings.call(document) do |value|
    if value.match?(/\$\{\{[^}]*\bsecrets\b/m) || value == "secrets"
      errors << "#{file}: workflow secret reference is forbidden"
    end
  end

  jobs = document["jobs"]
  unless jobs.is_a?(Hash)
    errors << "#{file}: jobs must be a mapping"
    next
  end
  if File.basename(file) == "ci.yml" && !jobs.key?(ATTEST_JOB)
    errors << "#{file}: required #{ATTEST_JOB} job is missing"
  end
  if File.basename(file) == "release.yml"
    ["build-release", RELEASE_ATTEST_JOB, RELEASE_PUBLISH_JOB].each do |required_job|
      errors << "#{file}: required #{required_job} job is missing" unless jobs.key?(required_job)
    end
  end

  jobs.each do |job_id, job|
    unless job.is_a?(Hash)
      errors << "#{file}: job #{job_id} must be a mapping"
      next
    end

    permission_errors.call(job["permissions"], file, "job #{job_id}")

    runners = job["runs-on"]
    runner_values = case runners
                    when String then [runners]
                    when Array then runners
                    when nil then []
                    else
                      errors << "#{file}: runner in job #{job_id} must be a string or sequence"
                      []
                    end
    runner_values.each do |runner|
      unless runner.is_a?(String)
        errors << "#{file}: runner label in job #{job_id} must be a string"
        next
      end
      runner_text = runner.to_s
      if runner_text.casecmp("self-hosted").zero? || runner_text.include?("${{")
        errors << "#{file}: forbidden or dynamic runner in job #{job_id}: #{runner_text.inspect}"
      end
    end

    if job.key?("continue-on-error") && ![false, "false"].include?(job["continue-on-error"])
      errors << "#{file}: continue-on-error must not bypass job #{job_id}"
    end

    job_uses = job["uses"]
    if job_uses
      ref = job_uses.to_s
      if ref.start_with?("./")
        errors << "#{file}: local reusable workflows are not supported: #{ref}"
      elsif !ref.match?(PINNED_ACTION)
        errors << "#{file}: reusable workflow is not pinned to a full commit SHA: #{ref}"
      end
    end

    ci_attestation_job = File.basename(file) == "ci.yml" && job_id.to_s == ATTEST_JOB
    release_attestation_job = File.basename(file) == "release.yml" && job_id.to_s == RELEASE_ATTEST_JOB
    release_publish_job = File.basename(file) == "release.yml" && job_id.to_s == RELEASE_PUBLISH_JOB
    protected_job = ci_attestation_job || release_attestation_job || release_publish_job
    steps = job["steps"]
    if steps.nil?
      errors << "#{file}: #{job_id} must define its fixed steps" if protected_job
      next
    end
    unless steps.is_a?(Array)
      errors << "#{file}: steps in job #{job_id} must be a sequence"
      next
    end

    steps.each_with_index do |step, index|
      unless step.is_a?(Hash)
        errors << "#{file}: step #{job_id}[#{index}] must be a mapping"
        next
      end

      if step.key?("continue-on-error") && ![false, "false"].include?(step["continue-on-error"])
        errors << "#{file}: continue-on-error must not bypass step #{job_id}[#{index}]"
      end

      next unless step.key?("uses")

      ref = step["uses"].to_s
      if ref.start_with?("./")
        errors << "#{file}: local actions are not supported: #{ref}"
        next
      end
      unless ref.match?(PINNED_ACTION)
        errors << "#{file}: external action is not pinned to a full commit SHA: #{ref}"
        next
      end

      next unless ref.start_with?("actions/checkout@")

      inputs = step["with"]
      persist_credentials = inputs.is_a?(Hash) ? inputs["persist-credentials"] : nil
      unless persist_credentials == false || persist_credentials.to_s.casecmp("false").zero?
        errors << "#{file}: checkout step #{job_id}[#{index}] must set persist-credentials: false"
      end
    end

    if ci_attestation_job
      expected_job_keys = %w[needs if runs-on permissions steps].sort
      unless job.keys.map(&:to_s).sort == expected_job_keys
        errors << "#{file}: #{ATTEST_JOB} contains unsupported job configuration"
      end

      unless job["if"].to_s.strip == "github.event_name == 'push'"
        errors << "#{file}: #{ATTEST_JOB} must be restricted to push events"
      end
      unless job["needs"].to_s == "package-smoke"
        errors << "#{file}: #{ATTEST_JOB} must depend directly on package-smoke"
      end
      unless job["runs-on"] == "ubuntu-24.04"
        errors << "#{file}: #{ATTEST_JOB} must run on ubuntu-24.04"
      end
      unless job["permissions"] == ATTEST_PERMISSIONS
        errors << "#{file}: #{ATTEST_JOB} must use the exact attestation permissions"
      end

      expected_steps = [
        {
          "uses" => DOWNLOAD_ARTIFACT_ACTION,
          "with" => {
            "name" => "solodock-embedded-package",
            "path" => "${{ runner.temp }}/attested-package"
          }
        },
        {
          "name" => "Attest package checksums",
          "uses" => ATTEST_ACTION,
          "with" => {
            "subject-path" => "${{ runner.temp }}/attested-package/solodock-package/SHA256SUMS"
          }
        }
      ]
      unless steps == expected_steps
        errors << "#{file}: #{ATTEST_JOB} must contain only the pinned download and attestation actions"
      end
    end

    if release_attestation_job
      expected = {
        "needs" => "build-release",
        "runs-on" => "ubuntu-24.04",
        "permissions" => ATTEST_PERMISSIONS,
        "steps" => [
          {
            "uses" => DOWNLOAD_ARTIFACT_ACTION,
            "with" => {
              "name" => "solodock-release-package",
              "path" => "${{ runner.temp }}/release"
            }
          },
          {
            "name" => "Attest release checksums",
            "uses" => ATTEST_ACTION,
            "with" => {
              "subject-path" => "${{ runner.temp }}/release/SHA256SUMS"
            }
          }
        ]
      }
      errors << "#{file}: #{RELEASE_ATTEST_JOB} must use the fixed isolated attestation job" unless job == expected
    end

    if release_publish_job
      publish_command = <<~'BASH'
        release_dir="$RUNNER_TEMP/release"
        asset="solodock-${GITHUB_REF_NAME}-ubuntu-24.04-x86_64.tar.gz"
        gh release create "$GITHUB_REF_NAME" \
          "$release_dir/$asset" \
          "$release_dir/SHA256SUMS" \
          "$release_dir/SOURCE_SHA" \
          --repo "$GITHUB_REPOSITORY" \
          --verify-tag \
          --generate-notes \
          --title "$GITHUB_REF_NAME"
      BASH
      expected = {
        "needs" => ["build-release", "attest-release"],
        "runs-on" => "ubuntu-24.04",
        "permissions" => { "contents" => "write" },
        "steps" => [
          {
            "uses" => DOWNLOAD_ARTIFACT_ACTION,
            "with" => {
              "name" => "solodock-release-package",
              "path" => "${{ runner.temp }}/release"
            }
          },
          {
            "name" => "Publish release assets",
            "env" => { "GH_TOKEN" => "${{ github.token }}" },
            "run" => publish_command
          }
        ]
      }
      errors << "#{file}: #{RELEASE_PUBLISH_JOB} must use the fixed isolated publishing job" unless job == expected
    end
  end
end

unless errors.empty?
  warn errors.uniq.join("\n")
  exit 1
end

puts "workflow security policy passed for #{workflow_files.length} files"
