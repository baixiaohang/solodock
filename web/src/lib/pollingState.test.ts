import { describe, expect, it } from 'vitest'
import { pollNeedsAttention, pollOutcomeText } from './pollingState'
import type { PollState } from './types'

function state(outcome: PollState['last_outcome']): PollState {
  return {
    app_id: 'app', generation: 'generation', enabled: true,
    consecutive_transient_failures: 0, next_check_not_before: null,
    last_checked_at: null, last_success_at: null,
    last_source_descriptor_digest: null, last_manifest_digest: null,
    last_platform: null, last_outcome: outcome, last_error_class: null,
    last_error_code: null, suppressed_target_key: null,
    suppressed_deployment_id: null, updated_at: '2026-08-29T00:00:00Z',
  }
}

describe('polling status presentation', () => {
  it('distinguishes no-op, coalescing and manual config states', () => {
    expect(pollOutcomeText(state('unchanged'))).toContain('未变化')
    expect(pollOutcomeText(state('busy_skipped'))).toContain('合并')
    expect(pollOutcomeText(state('config_pending_manual'))).toContain('手动部署')
  })

  it('highlights suppression and registry failures without treating no-op as an error', () => {
    expect(pollNeedsAttention(state('suppressed_failed_target'))).toBe(true)
    expect(pollNeedsAttention(state('credential_error'))).toBe(true)
    expect(pollNeedsAttention(state('unchanged'))).toBe(false)
    expect(pollOutcomeText(null)).toBe('尚未检查')
  })
})
