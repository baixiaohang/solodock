import { describe, expect, it } from 'vitest'

import { translateForLocale } from './i18n'
import { installationSummary } from './installationIdentity'

const english = (key: Parameters<typeof translateForLocale>[1], values?: Parameters<typeof translateForLocale>[2]) =>
  translateForLocale('en', key, values)
const chinese = (key: Parameters<typeof translateForLocale>[1], values?: Parameters<typeof translateForLocale>[2]) =>
  translateForLocale('zh-CN', key, values)

describe('installation identity presentation', () => {
  it('presents stable, main, development, and unknown identities without guessing fields', () => {
    expect(installationSummary({ channel: 'stable', version: '0.2.0', source_sha: 'abcdef0123456789abcdef0123456789abcdef01', package_identity: 'a'.repeat(64) }, english))
      .toBe('SoloDock v0.2.0 · stable · abcdef0')
    expect(installationSummary({ channel: 'main', version: 'main', source_sha: '1234567890abcdef1234567890abcdef12345678', package_identity: 'b'.repeat(64) }, chinese))
      .toBe('SoloDock main · 1234567')
    expect(installationSummary({ channel: 'development', version: '0.1.0', source_sha: null, package_identity: null }, english))
      .toBe('SoloDock 0.1.0 · development')
    expect(installationSummary({ channel: 'stable', version: null, source_sha: null, package_identity: null }, chinese))
      .toBe('SoloDock · unknown')
  })
})
