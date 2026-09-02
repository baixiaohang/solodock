// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import { afterEach, describe, expect, it } from 'vitest'

import HealthLifecycleEditor from './HealthLifecycleEditor.svelte'

afterEach(cleanup)

const limits = {
  running_stable_window_seconds: { min: 5, max: 300, default: 15 },
  http_interval_seconds: { min: 1, max: 300, default: 10 },
  http_timeout_seconds: { min: 1, max: 60, default: 5 },
  http_retries: { min: 1, max: 10, default: 6 },
  http_start_period_seconds: { min: 0, max: 300, default: 30 },
  stop_grace_period_seconds: { min: 1, max: 600, default: 10 },
}

describe('HealthLifecycleEditor', () => {
  it('完全使用 capabilities 提供的范围', () => {
    render(HealthLifecycleEditor, {
      health: { policy: 'healthy', http: { client: 'curl', scheme: 'http', host: '127.0.0.1', port: 3000, path: '/readyz', interval_seconds: 10, timeout_seconds: 5, retries: 6, start_period_seconds: 30 } },
      stopGrace: 60,
      limits,
    })
    expect(screen.getByLabelText('间隔（秒）').getAttribute('max')).toBe('300')
    expect(screen.getByLabelText('超时（秒）').getAttribute('max')).toBe('60')
    expect(screen.getByLabelText('重试次数').getAttribute('max')).toBe('10')
    expect(screen.getByLabelText('启动宽限（秒）').getAttribute('max')).toBe('300')
    expect(screen.getByLabelText('停机宽限（秒）').getAttribute('max')).toBe('600')
  })

  it('capabilities 不可用时 fail closed', () => {
    render(HealthLifecycleEditor, { health: { policy: 'running', stable_window_seconds: 15 }, stopGrace: 10, limits: null })
    expect(screen.getByText(/无法获取后端配置限制/)).toBeTruthy()
    expect((screen.getByRole('group', { name: '健康与生命周期' }) as HTMLFieldSetElement).disabled).toBe(true)
  })
})
