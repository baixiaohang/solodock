import type { PollState } from './types'
import { translate, type Translate } from './i18n'

const labels: Record<PollState['last_outcome'], Parameters<Translate>[0]> = {
  disabled: 'Disabled',
  scheduled: 'New digest scheduled',
  unchanged: 'Digest unchanged',
  config_pending_manual: 'Configuration changed; waiting for manual deployment',
  busy_skipped: 'Application busy; this poll was coalesced',
  blocked_drift: 'Runtime drift blocked deployment',
  blocked_attention: 'Administrator attention required',
  suppressed_failed_target: 'Failed digest suppressed',
  registry_error: 'Registry check failed',
  credential_error: 'Registry credential failed',
  invalid_source: 'Invalid image reference',
  cancelled: 'Check cancelled',
}

export function pollOutcomeText(state: PollState | null, translateMessage: Translate = translate): string {
  return state ? translateMessage(labels[state.last_outcome]) : translateMessage('Not checked yet')
}

export function pollNeedsAttention(state: PollState | null): boolean {
  return state !== null && [
    'blocked_drift',
    'blocked_attention',
    'suppressed_failed_target',
    'registry_error',
    'credential_error',
    'invalid_source',
  ].includes(state.last_outcome)
}
