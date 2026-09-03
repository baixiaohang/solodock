import { describe, expect, it } from 'vitest'

import { ApiError } from './api'
import { errorPresentation, errorPresentationText, FormValidationError, remapIndexedIssues } from './formErrors'

describe('form errors', () => {
  it('将服务端 issue 转换为可定位摘要并保留 request id', () => {
    const result = errorPresentation(new ApiError(422, {
      code: 'CONFIG_INVALID',
      message: 'The application configuration is invalid',
      request_id: 'request-id',
      issues: [{ path: 'binds[2].source', code: 'BIND_OUTSIDE_ALLOWED_ROOT', message: 'Must be below an allowed root' }],
    }), 'fallback')
    expect(errorPresentationText(result)).toContain('持久存储（binds[2].source）')
    expect(errorPresentationText(result)).toContain('request request-id')
    expect(result.issues).toHaveLength(1)
  })

  it('客户端确定性错误不需要 request id', () => {
    const result = errorPresentation(new FormValidationError([
      { path: 'environment.public[1].key', code: 'ENV_DUPLICATE', message: '变量名不能重复' },
    ]), 'fallback')
    expect(errorPresentationText(result)).toBe('环境变量（environment.public[1].key）：变量名不能重复')
  })

  it('按请求投影索引映射托管文件错误到可见行', () => {
    expect(remapIndexedIssues([
      { path: 'files[0].content', code: 'INVALID_FILE_CONTENT', message: 'invalid' },
      { path: 'files[2].logical_name', code: 'INVALID_FILE_NAME', message: 'invalid' },
    ], 'files', [1, -1, 0])).toEqual([
      { path: 'files[1].content', code: 'INVALID_FILE_CONTENT', message: 'invalid' },
      { path: 'files[0].logical_name', code: 'INVALID_FILE_NAME', message: 'invalid' },
    ])
  })
})
