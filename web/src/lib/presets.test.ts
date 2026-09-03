import { describe, expect, it } from 'vitest'

import { translateForLocale } from './i18n'
import { presetDescription } from './presets'

describe('preset presentation', () => {
  it('localizes a known preset by stable identity and preserves unknown canonical descriptions', () => {
    expect(presetDescription('postgresql', 'backend value', (key, values) => translateForLocale('zh-CN', key, values))).toContain('单实例 PostgreSQL')
    expect(presetDescription('future', 'Backend canonical description', (key, values) => translateForLocale('zh-CN', key, values))).toBe('Backend canonical description')
  })
})
