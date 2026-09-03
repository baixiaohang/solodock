import { describe, expect, it } from 'vitest'

import { translateForLocale } from './i18n'
import { mountKindText, networkKindText, networkModeText, stateText, transitionResultText } from './presentation'

const zh = (key: Parameters<typeof translateForLocale>[1], values?: Parameters<typeof translateForLocale>[2]) =>
  translateForLocale('zh-CN', key, values)

describe('known value presentation', () => {
  it('localizes known deployment, mount, and network values', () => {
    expect(stateText('verifying_rollback', zh)).toBe('验证回滚中')
    expect(transitionResultText('candidate_failed', zh)).toBe('候选版本失败')
    expect(mountKindText('tmpfs', zh)).toBe('临时文件系统')
    expect(networkModeText('owned_platform_and_external', zh)).toBe('应用专属、平台与外部网络')
    expect(networkKindText('owned_default', zh)).toBe('应用专属默认网络')
  })

  it('preserves unknown backend values verbatim', () => {
    expect(transitionResultText('future_result', zh)).toBe('future_result')
    expect(mountKindText('future_mount', zh)).toBe('future_mount')
    expect(networkModeText('future_mode', zh)).toBe('future_mode')
    expect(networkKindText('future_kind', zh)).toBe('future_kind')
  })
})
