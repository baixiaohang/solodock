import { describe, expect, it } from 'vitest'
import { canConfirmDeletion } from './deletionState'
import type { DeletionPreviewResponse } from './types'

const preview: DeletionPreviewResponse = {
  app_id: 'app', slug: 'example', expected_revision: 'revision', project_name: 'solodock-example',
  active_release_id: null, active_config_revision: null, pending_release_id: null, pending_config_revision: null,
  remove_container: false, container_ids: ['full-id'], managed_files: [{ logical_name: 'config', configured_in: 'draft' }],
  retained: { containers: ['full-id'], owned_volumes: [{ name: 'owned', configured_in: 'draft', exists: false }], external_volumes: [{ name: 'external', configured_in: 'draft', exists: true }], binds: [{ source: '/srv/data', readonly: true, configured_in: 'draft', exists: true }], networks: [{ name: 'network', configured_in: 'draft', exists: false }] },
  orphan_warning: true, webhook_configured: true, confirmation_token: 'write-only-token', expires_at: '2030-01-01T00:00:00Z',
}

describe('deletion confirmation state', () => {
  it('requires exact disposition, slug and an unexpired token', () => {
    const now = Date.parse('2029-12-31T23:59:59Z')
    expect(canConfirmDeletion(preview, 'example', false, now)).toBe(true)
    expect(canConfirmDeletion(preview, 'other', false, now)).toBe(false)
    expect(canConfirmDeletion(preview, 'example', true, now)).toBe(false)
    expect(canConfirmDeletion(preview, 'example', false, Date.parse(preview.expires_at))).toBe(false)
  })
})
