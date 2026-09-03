import { translate, type Translate } from './i18n'

const driftMessages = {
  DOCKER_UNAVAILABLE: 'Docker is temporarily unavailable',
  CONTAINER_MISSING: 'Container is missing',
  CONTAINER_AMBIGUOUS: 'Multiple candidate containers found',
  LABEL_INVALID: 'Container ownership label is invalid',
  ACTIVE_RELEASE_MISSING: 'Active release is missing',
  RELEASE_ID_MISMATCH: 'Running release does not match the active release',
  IMAGE_REF_MISMATCH: 'Running image does not match the active image',
  NETWORK_ATTACHMENT_MISMATCH: 'Actual network attachments do not match immutable release expectations',
  NETWORK_ALIAS_MISMATCH: 'Actual network is missing an expected alias',
  NETWORK_BRIDGE_IDENTITY_MISMATCH: 'Owned network driver or host bridge identity does not match expectations',
  PLATFORM_NETWORK_IDENTITY_MISMATCH: 'Platform service-discovery network is missing or has an unexpected identity',
} as const

export function driftText(code: string, translateMessage: Translate = translate): string {
  const key = driftMessages[code as keyof typeof driftMessages]
  return translateMessage(key ?? 'Unknown drift detected')
}

const stateMessages: Record<string, Parameters<Translate>[0]> = {
  ready: 'Ready', unavailable: 'Unavailable', starting: 'Starting', permission_denied: 'Permission denied', incompatible: 'Incompatible',
  healthy: 'Healthy', unhealthy: 'Unhealthy', running: 'Running', exited: 'Exited', unknown: 'Unknown',
  queued: 'Queued', succeeded: 'Succeeded', no_op: 'No operation', failed: 'Failed', rolled_back: 'Rolled back', needs_attention: 'Needs attention', interrupted: 'Interrupted',
  manual: 'Manual', rollback: 'Rollback', poll: 'Poll',
  ok: 'OK', degraded: 'Degraded', stopped: 'Stopped', normal: 'Normal', warning: 'Warning', critical: 'Critical',
  removing: 'Removing', paused: 'Paused', restarting: 'Restarting', dead: 'Dead', resolving: 'Resolving', preparing: 'Preparing', pulling: 'Pulling', applying: 'Applying', verifying: 'Verifying', committing: 'Committing', rolling_back: 'Rolling back', verifying_rollback: 'Verifying rollback', terminal: 'Terminal',
}

export function stateText(value: string | null | undefined, translateMessage: Translate = translate): string {
  if (!value) return translateMessage('Unavailable')
  const key = stateMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

const transitionResultMessages: Record<string, Parameters<Translate>[0]> = {
  scheduled: 'Scheduled', started: 'Started', candidate_published: 'Candidate published', image_verified: 'Image verified',
  candidate_applied: 'Candidate applied', health_passed: 'Health passed', committed: 'Committed', no_op: 'No operation',
  resolved: 'Resolved', rollback_target_verified: 'Rollback target verified', poll_target_verified: 'Poll target verified',
  pending_resumed: 'Pending release resumed', candidate_failed: 'Candidate failed', rollback_applied: 'Rollback applied',
  rolled_back: 'Rolled back', failed: 'Failed',
}

export function transitionResultText(value: string, translateMessage: Translate = translate): string {
  const key = transitionResultMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

const mountKindMessages: Record<string, Parameters<Translate>[0]> = {
  volume: 'Volume', bind: 'Bind mount', tmpfs: 'Temporary filesystem',
}

export function mountKindText(value: string, translateMessage: Translate = translate): string {
  const key = mountKindMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

const networkModeMessages: Record<string, Parameters<Translate>[0]> = {
  owned_only: 'Owned network only', owned_and_external: 'Owned and external networks', external_only: 'External networks only',
  owned_and_platform: 'Owned and platform networks', owned_platform_and_external: 'Owned, platform, and external networks',
  platform_and_external: 'Platform and external networks', platform_only: 'Platform network only',
}

export function networkModeText(value: string, translateMessage: Translate = translate): string {
  const key = networkModeMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

const networkKindMessages: Record<string, Parameters<Translate>[0]> = {
  owned_default: 'Owned default network', external: 'External network', platform: 'Platform network',
}

export function networkKindText(value: string, translateMessage: Translate = translate): string {
  const key = networkKindMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

const configuredScopeMessages: Record<string, Parameters<Translate>[0]> = {
  active: 'Active', pending: 'Pending', draft: 'Draft', active_and_pending: 'Active and pending',
  active_and_draft: 'Active and draft', pending_and_draft: 'Pending and draft', active_pending_and_draft: 'Active, pending, and draft',
}

export function configuredScopeText(value: string, translateMessage: Translate = translate): string {
  const key = configuredScopeMessages[value.toLowerCase()]
  return key ? translateMessage(key) : value
}

export function shortRef(value: string | null | undefined): string {
  if (!value) return '—'
  const digest = value.split('@sha256:')[1]
  return digest ? `sha256:${digest.slice(0, 12)}` : value.slice(0, 20)
}

export function formatBytes(value: number | null): string {
  if (value === null) return '—'
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let amount = value
  let index = 0
  while (amount >= 1024 && index < units.length - 1) { amount /= 1024; index += 1 }
  return `${amount.toFixed(index === 0 ? 0 : 1)} ${units[index]}`
}
