import { ApiError } from './api'

export interface FormIssue {
  path: string
  code: string
  message: string
}

export class FormValidationError extends Error {
  constructor(public issues: FormIssue[]) {
    super(issues[0]?.message ?? 'The form is invalid')
  }
}

export function issueAt(issues: FormIssue[], path: string): FormIssue | undefined {
  return issues.find((issue) => issue.path === path)
}

export function issuesUnder(issues: FormIssue[], prefix: string): FormIssue[] {
  return issues.filter((issue) => issue.path === prefix || issue.path.startsWith(`${prefix}.`) || issue.path.startsWith(`${prefix}[`))
}

function sectionName(path: string): string {
  if (path.startsWith('environment.')) return '环境变量'
  if (path.startsWith('files')) return '托管文件'
  if (path.startsWith('ports')) return '端口'
  if (path.startsWith('volumes') || path.startsWith('binds')) return '持久存储'
  if (path.startsWith('networks') || path.includes('network')) return '网络'
  if (path.startsWith('health') || path === 'stop_grace_period_seconds') return '健康与生命周期'
  if (path === 'discovery_image_ref' || path === 'credential_ref') return '镜像'
  return 'Draft 配置'
}

export function issueSummary(issue: FormIssue): string {
  return `${sectionName(issue.path)}（${issue.path}）：${issue.message}`
}

export function remapIndexedIssues(issues: FormIssue[], prefix: string, requestRowIndexes: number[]): FormIssue[] {
  const pattern = new RegExp(`^${prefix}\\[(\\d+)\\](.*)$`)
  return issues.map((issue) => {
    const match = pattern.exec(issue.path)
    if (!match) return issue
    const rowIndex = requestRowIndexes[Number(match[1])]
    return { ...issue, path: rowIndex >= 0 ? `${prefix}[${rowIndex}]${match[2]}` : prefix }
  })
}

export function errorPresentation(cause: unknown, fallback: string): { message: string; issues: FormIssue[]; requestId?: string } {
  if (cause instanceof FormValidationError) {
    return { message: issueSummary(cause.issues[0]), issues: cause.issues }
  }
  if (cause instanceof ApiError) {
    const issues = cause.body.issues ?? []
    const detail = issues[0] ? issueSummary(issues[0]) : `${cause.body.code}: ${cause.body.message}`
    return { message: `${detail}（request ${cause.body.request_id}）`, issues, requestId: cause.body.request_id }
  }
  return { message: fallback, issues: [] }
}
