import { describe, expect, it } from 'vitest'

import { EnvironmentTextError, parseEnvironmentText, serializeEnvironmentText } from './environmentText'

describe('environment text', () => {
  it('支持 CRLF、空行、空值和包含等号的 value，并可无损序列化', () => {
    const parsed = parseEnvironmentText('A=1\r\n\r\nB=\r\nTOKEN=a=b=')
    expect(parsed).toEqual([
      { key: 'A', value: '1' },
      { key: 'B', value: '' },
      { key: 'TOKEN', value: 'a=b=' },
    ])
    expect(serializeEnvironmentText(parsed)).toBe('A=1\nB=\nTOKEN=a=b=')
  })

  it.each([
    ['MISSING', 'ENV_TEXT_MISSING_SEPARATOR', 1],
    ['=value', 'ENV_KEY_REQUIRED', 1],
    ['BAD-KEY=value', 'ENV_KEY_INVALID', 1],
    ['A=1\nA=2', 'ENV_DUPLICATE', 2],
    ['TOKEN=public', 'ENV_SECRET_CONFLICT', 1],
  ])('拒绝无效文本 %#', (text, code, line) => {
    try {
      parseEnvironmentText(text, new Set(['TOKEN']))
      throw new Error('expected parser failure')
    } catch (cause) {
      expect(cause).toBeInstanceOf(EnvironmentTextError)
      expect((cause as EnvironmentTextError).issue.code).toBe(code)
      expect((cause as EnvironmentTextError).line).toBe(line)
    }
  })
})
