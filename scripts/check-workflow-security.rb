#!/usr/bin/env ruby
# frozen_string_literal: true

require "yaml"

PINNED_ACTION = %r{\A[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*@[0-9a-f]{40}\z}
DANGEROUS_TRIGGERS = %w[pull_request_target workflow_run].freeze

root = File.expand_path(ARGV.fetch(0, "."))
workflow_dir = File.join(root, ".github", "workflows")
abort "workflow directory not found: #{workflow_dir}" unless Dir.exist?(workflow_dir)

workflow_files = Dir.glob(File.join(workflow_dir, "*.{yml,yaml}")).sort
abort "no workflow files found in #{workflow_dir}" if workflow_files.empty?

errors = []

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
    errors << "#{file}: unapproved write permission in #{scope}: #{name}" unless allowed_codeql_upload
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

    steps = job["steps"]
    next if steps.nil?
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
  end
end

unless errors.empty?
  warn errors.uniq.join("\n")
  exit 1
end

puts "workflow security policy passed for #{workflow_files.length} files"
