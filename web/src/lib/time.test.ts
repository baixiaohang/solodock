import { describe, expect, it } from 'vitest'
import { browserSupportsTimezone, formatTimestamp } from './time'

describe('display time formatting', () => {
  it('formats UTC and Asia/Shanghai from the same UTC source', () => {
    const source = '2026-01-15T12:00:00Z'
    expect(formatTimestamp(source, 'UTC')).toContain('12:00:00')
    expect(formatTimestamp(source, 'Asia/Shanghai')).toContain('20:00:00')
  })

  it('honors daylight saving changes for IANA zones', () => {
    expect(formatTimestamp('2026-01-15T12:00:00Z', 'America/New_York')).toContain('07:00:00')
    expect(formatTimestamp('2026-07-15T12:00:00Z', 'America/New_York')).toContain('08:00:00')
  })

  it('detects unsupported browser zones and never uses local time implicitly', () => {
    expect(browserSupportsTimezone('UTC')).toBe(true)
    expect(browserSupportsTimezone('Mars/Olympus')).toBe(false)
    expect(formatTimestamp('2026-01-15T12:00:00Z', 'Mars/Olympus')).toContain('12:00:00')
  })

  it('formats timestamps with the selected UI locale', () => {
    const source = '2026-01-15T12:00:00Z'
    expect(formatTimestamp(source, 'UTC', 'en')).not.toBe(formatTimestamp(source, 'UTC', 'zh-CN'))
  })
})
