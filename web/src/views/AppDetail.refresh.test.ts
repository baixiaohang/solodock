// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import AppDetail from './AppDetail.svelte'
import { ApiError, api, mutation } from '../lib/api'

vi.mock('../lib/api', async (importOriginal) => ({ ...await importOriginal<typeof import('../lib/api')>(), api: vi.fn(), mutation: vi.fn() }))
vi.mock('../lib/sse', () => ({ openSse: () => ({ close: vi.fn() }) }))

const read = vi.mocked(api)
const write = vi.mocked(mutation)
const limits = {
  running_stable_window_seconds: { min: 5, max: 300, default: 15 },
  http_interval_seconds: { min: 1, max: 300, default: 10 },
  http_timeout_seconds: { min: 1, max: 60, default: 5 },
  http_retries: { min: 1, max: 10, default: 6 },
  http_start_period_seconds: { min: 0, max: 300, default: 30 },
  stop_grace_period_seconds: { min: 1, max: 600, default: 10 },
}
function application() {
  return {
    id: 'app-id', slug: 'refresh-demo', display_name: 'Refresh demo',
    resource_names: { project_name: 'solodock-refresh-demo' }, active_release: null, actual_release_id: null,
    actual: null, expected_network_plan: null, expected_owned_default_network: null, actual_owned_default_network: null,
    drift_codes: [], draft_revision: 'revision-one', draft_config_sha256: 'a'.repeat(64), active_config_revision: null,
    pending_release_id: null, pending_image_ref: null, desired_state: 'stopped', deployment_status: 'DEPLOY_REQUIRED',
    available_actions: ['deploy'], compose_available: true, polling: null,
    draft: { discovery_image_ref: 'registry.example/app:stable', credential_ref: 'credential-one',
      auto_deploy_enabled: false, poll_interval_seconds: 300, stop_grace_period_seconds: 10,
      public_environment: [{ key: 'PUBLIC', value: 'old' }], secret_keys: ['TOKEN'], files: [],
      ports: [], volumes: [], binds: [], owned_default_network: true, service_discovery_enabled: false,
      networks: [], health: { policy: 'running', stable_window_seconds: 15 } },
  }
}
let current: any
let history: any[]
let failures: Map<string, unknown>
function response(path: string): unknown {
  if (failures.has(path)) throw failures.get(path)
  if (path.endsWith('/deployments?limit=20')) return { items: history }
  if (path.endsWith('/registry-credentials')) return [{ id: 'credential-one', registry: 'registry.example', username: 'user' }]
  if (path.endsWith('/settings')) return { allowed_bind_roots: [], configuration_limits: { health: limits } }
  if (path.endsWith('/webhook')) return { configured: false }
  return structuredClone(current)
}
async function settle() { await act(async () => { await vi.advanceTimersByTimeAsync(0) }) }
async function mount() { const view = render(AppDetail, { appId: 'app-id' }); await settle(); return view }
async function poll() { await act(async () => { await vi.advanceTimersByTimeAsync(10_000) }) }
const conflict = () => new ApiError(409, { code: 'DEPLOYMENT_FACTS_CHANGED', message: 'Changed', request_id: '' }, 'known_not_applied')

beforeEach(() => {
  vi.useFakeTimers()
  current = application(); history = []; failures = new Map()
  read.mockReset(); write.mockReset()
  read.mockImplementation(async (path) => response(path) as never)
  write.mockResolvedValue({ app: { config_revision: 'revision-two' } } as never)
  vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('visible')
})
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks() })

describe('app detail refresh and loading recovery', () => {
  it('hides disabled webhooks without a warning or repeated polling requests', async () => {
    failures.set('/api/v1/apps/app-id/webhook', new ApiError(501, { code: 'WEBHOOK_UNAVAILABLE', message: 'The webhook endpoint is not configured', request_id: '' }))
    await mount()
    expect(screen.getByRole('heading', { name: 'Refresh demo' })).toBeTruthy()
    expect(screen.queryByText('无法加载 Webhook 设置。')).toBeNull()
    expect(screen.queryByRole('button', { name: '重试加载' })).toBeNull()
    await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    expect(screen.queryByRole('heading', { name: 'Registry recheck webhook' })).toBeNull()
    await poll()
    expect(read.mock.calls.filter(([path]) => path.endsWith('/webhook'))).toHaveLength(1)
  })

  it.each([
    new Error('offline'),
    new ApiError(503, { code: 'WEBHOOK_STORE_DEGRADED', message: 'Unavailable', request_id: '' }),
    new ApiError(501, { code: 'HTTP_ERROR', message: 'Not implemented', request_id: '' }),
    new ApiError(500, { code: 'WEBHOOK_UNAVAILABLE', message: 'Failed', request_id: '' }),
  ])('keeps real webhook failures visible and allows retry (%s)', async (cause) => {
    failures.set('/api/v1/apps/app-id/webhook', cause)
    await mount()
    expect(screen.getByText('无法加载 Webhook 设置。')).toBeTruthy()
    failures.clear()
    await fireEvent.click(screen.getByRole('button', { name: '重试加载' })); await settle()
    expect(screen.queryByText('无法加载 Webhook 设置。')).toBeNull()
    await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    expect(screen.getByRole('heading', { name: 'Registry recheck webhook' })).toBeTruthy()
  })

  it('clears a stale webhook panel and warning when a retry reports disabled webhooks', async () => {
    failures.set('/api/v1/apps/app-id/deployments?limit=20', new Error('offline'))
    await mount()
    await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    expect(screen.getByRole('heading', { name: 'Registry recheck webhook' })).toBeTruthy()
    failures.clear()
    failures.set('/api/v1/apps/app-id/webhook', new Error('offline'))
    await fireEvent.click(screen.getByRole('button', { name: '重试加载' })); await settle()
    expect(screen.getByText('无法加载 Webhook 设置。')).toBeTruthy()
    failures.set('/api/v1/apps/app-id/webhook', new ApiError(501, { code: 'WEBHOOK_UNAVAILABLE', message: 'The webhook endpoint is not configured', request_id: '' }))
    await fireEvent.click(screen.getByRole('button', { name: '重试加载' })); await settle()
    expect(screen.queryByText('无法加载 Webhook 设置。')).toBeNull()
    expect(screen.queryByRole('heading', { name: 'Registry recheck webhook' })).toBeNull()
  })

  it.each([
    [new Error('offline'), '无法加载应用。'],
    [new ApiError(500, { code: 'INTERNAL', message: 'Failed', request_id: '' }), '无法加载应用。'],
    [new ApiError(404, { code: 'APP_NOT_FOUND', message: 'Missing', request_id: '' }), '应用不存在。'],
  ])('shows initial failure and retries successfully (%s)', async (cause, message) => {
    failures.set('/api/v1/apps/app-id', cause)
    await mount()
    expect(screen.getByText(message)).toBeTruthy()
    failures.clear()
    await fireEvent.click(screen.getByRole('button', { name: '重试加载' })); await settle()
    expect(screen.getByRole('heading', { name: 'Refresh demo' })).toBeTruthy()
    expect(screen.queryByText(message)).toBeNull()
  })

  it('isolates auxiliary failures, keeps credential selection, and blocks saving without capabilities', async () => {
    for (const path of ['/api/v1/registry-credentials', '/api/v1/settings', '/api/v1/apps/app-id/deployments?limit=20']) failures.set(path, new Error('offline'))
    await mount()
    expect(screen.getByRole('heading', { name: 'Refresh demo' })).toBeTruthy()
    await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', true)
    expect(screen.getByLabelText('Registry credential')).toHaveProperty('value', 'credential-one')
    failures.clear()
    await fireEvent.click(screen.getByRole('button', { name: '重试加载' })); await settle()
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', false)
    expect(screen.getByLabelText('Registry credential')).toHaveProperty('value', 'credential-one')
  })

  it('refreshes automatic deployment facts, history, and action availability', async () => {
    await mount()
    current.actual_release_id = 'release-new'; current.pending_release_id = 'pending-new'
    current.available_actions = ['deploy', 'start']
    history = [{ id: 'deployment-new', status: 'succeeded', phase: 'terminal', trigger: 'poll', created_at: '2026-09-01T00:00:00Z' }]
    await poll()
    expect(screen.getByRole('button', { name: '启动' })).toHaveProperty('disabled', false)
    await fireEvent.click(screen.getByRole('button', { name: '部署历史' }))
    expect(screen.getByRole('link', { name: '查看详情' }).getAttribute('href')).toBe('#/deployments/deployment-new')
    write.mockResolvedValue({ deployment_id: 'deployment-new' } as never)
    await fireEvent.click(screen.getByRole('button', { name: '部署 draft' })); await settle()
    expect(write).toHaveBeenCalledWith('/api/v1/apps/app-id/deployments', expect.objectContaining({ expected_actual_release_id: 'release-new', expected_pending_release_id: 'pending-new' }), expect.anything())
  })

  it('preserves fields, Secret input, invalid bulk text, errors, and the editing revision', async () => {
    await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'Local edit' } })
    await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
    const bulk = screen.getByLabelText('批量普通环境变量')
    await fireEvent.input(bulk, { target: { value: 'PUBLIC=new\nINVALID' } })
    await fireEvent.input(screen.getByLabelText('Secret 值'), { target: { value: 'replacement' } })
    current.draft_revision = 'revision-two'; current.display_name = 'Remote edit'
    await poll()
    expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'Local edit')
    expect(screen.getByLabelText('Secret 值')).toHaveProperty('value', 'replacement')
    expect(bulk).toHaveProperty('value', 'PUBLIC=new\nINVALID')
    expect(screen.getByText('第 2 行缺少 =')).toBeTruthy()
    expect(screen.getByRole('button', { name: '重新载入 draft' })).toBeTruthy()
    await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
    expect(write).not.toHaveBeenCalled()
    await fireEvent.input(bulk, { target: { value: 'PUBLIC=new' } })
    write.mockRejectedValue(conflict())
    await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
    expect(write).toHaveBeenCalledWith('/api/v1/apps/app-id/draft', expect.objectContaining({ expected_revision: 'revision-one' }), expect.anything())
    expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'Local edit')
  })

  it('preserves last good data on background failure and distinguishes unknown retries from conflicts', async () => {
    await mount()
    failures.set('/api/v1/apps/app-id', new Error('offline')); await poll()
    expect(screen.getByRole('heading', { name: 'Refresh demo' })).toBeTruthy()
    expect(screen.getByText('无法刷新应用，当前显示上次加载的状态。')).toBeTruthy()
    failures.clear(); await poll()
    write.mockRejectedValue(new Error('response lost'))
    await fireEvent.click(screen.getByRole('button', { name: '部署 draft' })); await settle()
    const initial = write.mock.calls[0]
    current.draft_revision = 'revision-two'; current.available_actions = []
    await poll()
    await fireEvent.click(screen.getByRole('button', { name: '部署 draft' })); await settle()
    expect(write.mock.calls[1]).toEqual(initial)
    write.mockRejectedValue(conflict())
    await fireEvent.click(screen.getByRole('button', { name: '部署 draft' })); await settle()
    current.available_actions = ['deploy']; await poll()
    await fireEvent.click(screen.getByRole('button', { name: '部署 draft' })); await settle()
    expect(write.mock.calls[3][1]).toMatchObject({ expected_draft_revision: 'revision-two' })
    expect(write.mock.calls[3][2]?.idempotencyKey).not.toBe(initial[2]?.idempotencyKey)
  })

  it('does not overlap slow polls and ignores superseded or disposed responses', async () => {
    const view = await mount()
    let finish: ((value: unknown) => void) | undefined
    let signal: AbortSignal | undefined
    read.mockImplementation((path, init) => {
      if (path === '/api/v1/apps/app-id') { signal = init?.signal as AbortSignal; return new Promise((resolve) => { finish = resolve }) as never }
      return Promise.resolve(response(path)) as never
    })
    await poll()
    const appReads = () => read.mock.calls.filter(([path]) => path === '/api/v1/apps/app-id').length
    const calls = appReads()
    await act(async () => { await vi.advanceTimersByTimeAsync(5_000) })
    expect(appReads()).toBe(calls)
    current.display_name = 'New result'
    read.mockImplementation(async (path) => response(path) as never)
    await fireEvent(document, new Event('visibilitychange')); await settle()
    expect(signal?.aborted).toBe(true)
    await act(async () => { finish?.({ ...current, display_name: 'Stale result' }) }); await settle()
    expect(screen.getByRole('heading', { name: 'New result' })).toBeTruthy()
    view.unmount(); const afterUnmount = read.mock.calls.length
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000) })
    expect(read.mock.calls).toHaveLength(afterUnmount)
    expect(vi.getTimerCount()).toBe(0)
  })
})

describe('editing session and auxiliary cancellation boundaries', () => {
  it.each(['PUBLIC=local', 'PUBLIC=local\nINVALID'])('explicit reload replaces the whole bulk editor session (%s)', async (text) => {
    await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
    await fireEvent.input(screen.getByLabelText('批量普通环境变量'), { target: { value: text } })
    current.draft_revision = 'revision-two'; current.draft.public_environment[0].value = 'remote'
    await poll()
    await fireEvent.click(screen.getByRole('button', { name: '重新载入 draft' }))
    expect(screen.getByDisplayValue('remote')).toBeTruthy()
    expect(screen.queryByLabelText('批量普通环境变量')).toBeNull()
    expect(screen.queryByText('第 2 行缺少 =')).toBeNull()
    await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
    expect(screen.getByLabelText('批量普通环境变量')).toHaveProperty('value', 'PUBLIC=remote')
    write.mockRejectedValue(new ApiError(422, { code: 'CONFIG_INVALID', message: 'Fixture rejection', request_id: '' }, 'known_not_applied'))
    for (const name of ['仅预检', '保存新 revision']) {
      await fireEvent.click(screen.getByRole('button', { name })); await settle()
    }
    for (const call of write.mock.calls) expect(call[1]).toMatchObject({ draft: { environment: { public: [{ key: 'PUBLIC', value: 'remote' }] } } })
    expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-two' })
  })

  it('restarts unfinished initial settings and credentials when visibility supersedes their load', async () => {
    const pending: { signal: AbortSignal; resolve: (value: unknown) => void }[] = []
    read.mockImplementation((path, init) => {
      if (path.endsWith('/settings') || path.endsWith('/registry-credentials')) return new Promise((resolve) => {
        pending.push({ signal: init?.signal as AbortSignal, resolve })
      }) as never
      return Promise.resolve(response(path)) as never
    })
    await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', true)
    read.mockImplementation(async (path) => response(path) as never)
    await fireEvent(document, new Event('visibilitychange')); await settle()
    expect(pending).toHaveLength(2)
    expect(pending.every((request) => request.signal.aborted)).toBe(true)
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', false)
    await act(async () => { pending[0].resolve([]); pending[1].resolve({ allowed_bind_roots: [] }) }); await settle()
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', false)
    expect(screen.getByLabelText('Registry credential')).toHaveProperty('value', 'credential-one')
  })

  it('keeps saving busy through its refresh and does not reset inputs entered during a slow auxiliary read', async () => {
    await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
    let finish: ((value: unknown) => void) | undefined
    current.draft_revision = 'revision-two'
    read.mockImplementation((path) => {
      if (path.endsWith('/settings')) return new Promise((resolve) => { finish = resolve }) as never
      return Promise.resolve(response(path)) as never
    })
    await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', true)
    await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'Next unsaved input' } })
    await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
    await fireEvent.input(screen.getByLabelText('批量普通环境变量'), { target: { value: 'PUBLIC=next\nINVALID' } })
    const reads = read.mock.calls.length
    await fireEvent(document, new Event('visibilitychange')); await settle()
    expect(read.mock.calls).toHaveLength(reads)
    await act(async () => { finish?.({ allowed_bind_roots: [], configuration_limits: { health: limits } }) }); await settle()
    expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'Next unsaved input')
    expect(screen.getByLabelText('批量普通环境变量')).toHaveProperty('value', 'PUBLIC=next\nINVALID')
    expect(screen.getByRole('button', { name: '保存新 revision' })).toHaveProperty('disabled', false)
    expect(screen.queryByRole('button', { name: '重新载入 draft' })).toBeNull()
  })
})

it.each([false, true])('clears only confirmed submitted sensitive values while preserving concurrent edits (new sensitive input: %s)', async (replaceDuringSave) => {
  current.draft.files = [{ logical_name: 'key', target_path: '/key.pem', sensitive: true }]
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
  const secret = screen.getByLabelText('Secret 值')
  const file = screen.getByLabelText('内容')
  await fireEvent.input(secret, { target: { value: 'submitted-token' } })
  await fireEvent.input(file, { target: { value: '-----BEGIN PRIVATE KEY-----\nsubmitted\n' } })
  let finish: ((value: unknown) => void) | undefined
  write.mockImplementation(() => new Promise((resolve) => { finish = resolve }) as never)
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'Unsaved next name' } })
  if (replaceDuringSave) {
    await fireEvent.input(secret, { target: { value: 'unsubmitted-token' } })
    await fireEvent.input(file, { target: { value: 'unsubmitted\nfile\n' } })
  }
  current.draft_revision = 'revision-two'
  await act(async () => { finish?.({ app: { config_revision: 'revision-two' } }) }); await settle()
  expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'Unsaved next name')
  expect(secret).toHaveProperty('value', replaceDuringSave ? 'unsubmitted-token' : '')
  expect(file).toHaveProperty('value', replaceDuringSave ? 'unsubmitted\nfile\n' : '')
  expect(screen.queryByRole('button', { name: '重新载入 draft' })).toBeNull()
  write.mockResolvedValue({ app: { config_revision: 'revision-three' } } as never)
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-two', draft: {
    environment: { secrets: [{ key: 'TOKEN', operation: replaceDuringSave ? 'replace' : 'keep' }] },
    files: [{ logical_name: 'key', operation: replaceDuringSave ? 'replace' : 'keep' }],
  } })
})

it.each(['success', 'refresh failure', 'external revision'])('advances the confirmed save baseline with concurrent edits (%s)', async (outcome) => {
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  let finish: ((value: unknown) => void) | undefined
  write.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }) as never)
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'Next name' } })
  current.draft_revision = outcome === 'external revision' ? 'revision-three' : 'revision-two'
  if (outcome === 'refresh failure') failures.set('/api/v1/apps/app-id', new Error('offline'))
  await act(async () => { finish?.({ app: { config_revision: 'revision-two' } }) }); await settle()
  expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'Next name')
  expect(screen.queryByRole('button', { name: '重新载入 draft' }) !== null).toBe(outcome === 'external revision')
  if (outcome === 'external revision') write.mockRejectedValueOnce(conflict())
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-two', draft: { display_name: 'Next name' } })
  if (outcome === 'external revision') expect(screen.getByRole('button', { name: '重新载入 draft' })).toBeTruthy()
})

it('preserves invalid bulk text entered during saving across tab remounts and blocks stale submission', async () => {
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  let finish: ((value: unknown) => void) | undefined
  write.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }) as never)
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  await fireEvent.click(screen.getByRole('button', { name: '批量文本' }))
  await fireEvent.input(screen.getByLabelText('批量普通环境变量'), { target: { value: 'PUBLIC=new\nINVALID' } })
  await fireEvent.click(screen.getByRole('button', { name: '概览' }))
  current.draft_revision = 'revision-two'
  await act(async () => { finish?.({ app: { config_revision: 'revision-two' } }) }); await settle()
  await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  expect(screen.getByLabelText('批量普通环境变量')).toHaveProperty('value', 'PUBLIC=new\nINVALID')
  for (const name of ['保存新 revision', '仅预检']) { await fireEvent.click(screen.getByRole('button', { name })); await settle() }
  expect(write).toHaveBeenCalledTimes(1)
  await fireEvent.input(screen.getByLabelText('批量普通环境变量'), { target: { value: 'PUBLIC=new' } })
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-two', draft: { environment: { public: [{ key: 'PUBLIC', value: 'new' }] } } })
})

it('does not adopt a revision from refresh after an unknown save and reuses the unchanged retry identity', async () => {
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  write.mockRejectedValue(new TypeError('offline'))
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  current.draft_revision = 'revision-two'
  await poll()
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-one' })
  expect(write.mock.calls[1][2]).toEqual(write.mock.calls[0][2])
  expect(screen.getByRole('button', { name: '重新载入 draft' })).toBeTruthy()
})

it('does not overwrite a new editing session when a cancelled session finishes saving', async () => {
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  let finish: ((value: unknown) => void) | undefined
  write.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve }) as never)
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  await fireEvent.click(screen.getByRole('button', { name: '取消' }))
  await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'New session' } })
  current.draft_revision = 'revision-two'
  await act(async () => { finish?.({ app: { config_revision: 'revision-two' } }) }); await settle()
  expect(screen.getByLabelText('显示名称')).toHaveProperty('value', 'New session')
  expect(screen.getByRole('button', { name: '重新载入 draft' })).toBeTruthy()
  await fireEvent.click(screen.getByRole('button', { name: '保存新 revision' })); await settle()
  expect(write.mock.calls[1][1]).toMatchObject({ expected_revision: 'revision-one' })
})

it.each([false, true])('ignores a late preflight rejection after editing or starting a new session (new session: %s)', async (newSession) => {
  await mount(); await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  let reject: ((cause: unknown) => void) | undefined
  write.mockImplementationOnce(() => new Promise((_, fail) => { reject = fail }) as never)
  await fireEvent.click(screen.getByRole('button', { name: '仅预检' })); await settle()
  if (newSession) {
    await fireEvent.click(screen.getByRole('button', { name: '取消' }))
    await fireEvent.click(screen.getByRole('button', { name: '配置' }))
  } else {
    await fireEvent.input(screen.getByLabelText('显示名称'), { target: { value: 'New input' } })
  }
  await act(async () => { reject?.(new ApiError(422, {
    code: 'CONFIG_INVALID', message: 'Old preflight error', request_id: 'old-preflight',
    issues: [{ path: 'display_name', code: 'INVALID_NAME', message: 'Old preflight error' }],
  }, 'known_not_applied')) }); await settle()
  expect(screen.queryByText(/Old preflight error/)).toBeNull()
  expect(screen.getByLabelText('显示名称').getAttribute('aria-invalid')).toBeNull()
  expect(screen.getByRole('button', { name: '仅预检' })).toHaveProperty('disabled', false)
})
