import { ApiError } from './api'
import { messageText, translate, type Translate, type UserMessage } from './i18n'
import { LocalMutationValidationError } from './mutationState'

export interface FormIssue {
  path: string
  code: string
  message: UserMessage
}

export class FormValidationError extends LocalMutationValidationError {
  constructor(public issues: FormIssue[]) {
    super(issues[0] ? messageText(issues[0].message) : 'The form is invalid')
  }
}

export function issueAt(issues: FormIssue[], path: string): FormIssue | undefined {
  return issues.find((issue) => issue.path === path)
}

export function issuesUnder(issues: FormIssue[], prefix: string): FormIssue[] {
  return issues.filter((issue) => issue.path === prefix || issue.path.startsWith(`${prefix}.`) || issue.path.startsWith(`${prefix}[`))
}

function sectionName(path: string, translateMessage: Translate): string {
  if (path.startsWith('environment.')) return translateMessage('Environment variables')
  if (path.startsWith('files')) return translateMessage('Managed files')
  if (path.startsWith('ports')) return translateMessage('Ports')
  if (path.startsWith('volumes') || path.startsWith('binds')) return translateMessage('Persistent storage')
  if (path.startsWith('networks') || path.includes('network')) return translateMessage('Networks')
  if (path.startsWith('health') || path === 'stop_grace_period_seconds') return translateMessage('Health and lifecycle')
  if (path === 'discovery_image_ref' || path === 'credential_ref') return translateMessage('Image')
  return translateMessage('Draft configuration')
}

export function issueSummary(issue: FormIssue, translateMessage: Translate = translate): string {
  return translateMessage('{section} ({path}): {message}', { section: sectionName(issue.path, translateMessage), path: issue.path, message: messageText(issue.message, translateMessage) })
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

export interface ErrorPresentation {
  fallback: UserMessage
  issues: FormIssue[]
  requestId?: string
  detail?: string
}

export function errorPresentation(cause: unknown, fallback: UserMessage): ErrorPresentation {
  if (cause instanceof FormValidationError) {
    return { fallback, issues: cause.issues }
  }
  if (cause instanceof ApiError) {
    const issues = cause.body.issues ?? []
    return {
      fallback,
      issues,
      detail: issues.length ? undefined : `${cause.body.code}: ${cause.body.message}`,
      requestId: cause.body.request_id,
    }
  }
  return { fallback, issues: [] }
}

export function errorPresentationText(presentation: ErrorPresentation, translateMessage: Translate = translate): string {
  const detail = presentation.issues[0]
    ? issueSummary(presentation.issues[0], translateMessage)
    : presentation.detail ?? messageText(presentation.fallback, translateMessage)
  return presentation.requestId
    ? translateMessage('{detail} (request {requestId})', { detail, requestId: presentation.requestId })
    : detail
}
