// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, expect, it, vi } from 'vitest'
import RetentionSettings from './RetentionSettings.svelte'
const policy = { enabled: false, keep_versions: 3, revision: '00000000-0000-0000-0000-000000000000', last_checked_at: null, last_status: null, last_error_code: null }
const response = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } })
afterEach(() => { cleanup(); vi.unstubAllGlobals() })
it('enables a policy with its loaded revision and preserves the same request after an unknown result', async () => {
  const fetch = vi.fn().mockResolvedValueOnce(response(policy)).mockRejectedValueOnce(new TypeError('network')).mockResolvedValueOnce(response({ ...policy, enabled: true, revision: 'saved' }))
  vi.stubGlobal('fetch', fetch)
  const user = userEvent.setup()
  render(RetentionSettings, { appId: 'app' })
  const box = await screen.findByRole('checkbox')
  expect((box as HTMLInputElement).checked).toBe(false)
  await user.click(box)
  await user.click(screen.getByRole('button', { name: '保存清理策略' }))
  await screen.findByRole('alert')
  expect((box as HTMLInputElement).disabled).toBe(true)
  await user.click(screen.getByRole('button', { name: '重试同一次策略修改' }))
  await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3))
  const first = fetch.mock.calls[1]![1]
  const retry = fetch.mock.calls[2]![1]
  expect(first.body).toBe(retry.body)
  expect(first.headers['Idempotency-Key']).toBe(retry.headers['Idempotency-Key'])
  expect(JSON.parse(first.body)).toEqual({ expected_revision: policy.revision, enabled: true, keep_versions: 3 })
})
it('reports a blocked image phase separately from saved policy', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ ...policy, enabled: true, last_status: 'blocked', last_error_code: 'IMAGE_CLEANUP_INVENTORY_INCOMPLETE' })))
  render(RetentionSettings, { appId: 'app' })
  await screen.findByText('发布文件阶段已完成，但无法确认镜像清理安全性。镜像会保留并等待安全重试。')
  expect(screen.getByRole('link', { name: '预览手动存储与镜像清理' }).getAttribute('href')).toBe('#/settings')
})
