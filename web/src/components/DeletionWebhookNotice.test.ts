// @vitest-environment jsdom
import { render, screen } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import DeletionWebhookNotice from './DeletionWebhookNotice.svelte'

describe('deletion webhook notice', () => {
  it('renders the irreversible secret deletion fact only for configured hooks', () => {
    const rendered = render(DeletionWebhookNotice, { configured: true })
    expect(screen.getByRole('alert').textContent).toContain('webhook secret')
    rendered.unmount()
    render(DeletionWebhookNotice, { configured: false })
    expect(screen.queryByRole('alert')).toBeNull()
  })
})
