// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import NewApp from './NewApp.svelte'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('new app resource preview', () => {
  it('hides owned network and bridge identity after switching to external-only', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('[]', { status: 200 })))
    const user = userEvent.setup()
    render(NewApp)

    await user.type(screen.getByLabelText(/^Slug/), 'demo')
    expect(screen.getByText('solodock-demo-app-1')).toBeTruthy()
    expect(screen.getByText('solodock-demo-default')).toBeTruthy()
    expect(screen.getByText('sd-demo')).toBeTruthy()

    await user.click(screen.getByLabelText('创建应用专属默认网络'))
    await waitFor(() => {
      expect(screen.queryByText('solodock-demo-default')).toBeNull()
      expect(screen.queryByText('sd-demo')).toBeNull()
    })
    expect(screen.getByText('solodock-demo-app-1')).toBeTruthy()
  })
})
