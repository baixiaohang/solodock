import type { FormIssue } from './formErrors'
import { localized, messageText, type UserMessage } from './i18n'

export interface PublicEnvironmentEntry {
  key: string
  value: string
}

export class EnvironmentTextError extends Error {
  constructor(public issue: FormIssue, public line: number) {
    super(messageText(issue.message))
  }
}

const VALID_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/

function fail(line: number, code: string, message: UserMessage): never {
  throw new EnvironmentTextError({ path: `environment.public[${line - 1}].key`, code, message }, line)
}

export function parseEnvironmentText(text: string, secretKeys: ReadonlySet<string> = new Set()): PublicEnvironmentEntry[] {
  const result: PublicEnvironmentEntry[] = []
  const seen = new Set<string>()
  for (const [index, rawLine] of text.split(/\r?\n/).entries()) {
    const line = index + 1
    if (!rawLine.trim()) continue
    const separator = rawLine.indexOf('=')
    if (separator < 0) fail(line, 'ENV_TEXT_MISSING_SEPARATOR', localized('Line {line} is missing =', { line }))
    const key = rawLine.slice(0, separator).trim()
    const value = rawLine.slice(separator + 1)
    if (!key) fail(line, 'ENV_KEY_REQUIRED', localized('The variable name on line {line} is required', { line }))
    if (!VALID_KEY.test(key)) fail(line, 'ENV_KEY_INVALID', localized('The variable name on line {line} is invalid', { line }))
    if (seen.has(key)) fail(line, 'ENV_DUPLICATE', localized('The variable name on line {line} is duplicated', { line }))
    if (secretKeys.has(key)) fail(line, 'ENV_SECRET_CONFLICT', localized('The variable on line {line} conflicts with a stored secret', { line }))
    seen.add(key)
    result.push({ key, value })
  }
  return result
}

export function serializeEnvironmentText(entries: PublicEnvironmentEntry[]): string {
  return entries.map(({ key, value }) => `${key}=${value}`).join('\n')
}
