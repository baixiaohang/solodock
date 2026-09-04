import { describe, expect, it } from 'vitest'
import { appendBounded, appendDeduplicatedLog } from './sse'
import { driftText } from './presentation'
import type { LogEvent } from './types'

describe('bounded SSE state', () => {
  it('keeps only the newest 1000 log lines and deduplicates event IDs', () => {
    let logs: Array<LogEvent & { id: string }> = Array.from({ length: 1000 }, (_, index) => ({ id: String(index), timestamp: '', stream: 'stdout', message: String(index), truncated: false }))
    const duplicate = appendDeduplicatedLog(logs, logs[999])
    expect(duplicate).toBe(logs)
    logs = appendDeduplicatedLog(logs, { id: '1000', timestamp: '', stream: 'stderr', message: 'next', truncated: false })
    expect(logs).toHaveLength(1000)
    expect(logs[0].id).toBe('1')
    expect(logs[999].message).toBe('next')
  })

  it('provides stable drift text and generic unknown fallback', () => {
    expect(driftText('IMAGE_REF_MISMATCH')).toContain('镜像')
    expect(driftText('DEPLOYMENT_PENDING')).toBe('存在待处理的版本部署')
    expect(driftText('ORPHAN_CONTAINER')).toBe('检测到孤立的托管容器')
    expect(driftText('FUTURE_CODE')).toBe('检测到未知漂移')
    expect(appendBounded([1, 2], 3, 2)).toEqual([2, 3])
  })
})
