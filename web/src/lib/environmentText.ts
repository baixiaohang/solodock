import type { FormIssue } from './formErrors'

export interface PublicEnvironmentEntry {
  key: string
  value: string
}

export class EnvironmentTextError extends Error {
  constructor(public issue: FormIssue, public line: number) {
    super(issue.message)
  }
}

const VALID_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/

function fail(line: number, code: string, message: string): never {
  throw new EnvironmentTextError({ path: `environment.public[${line - 1}].key`, code, message }, line)
}

export function parseEnvironmentText(text: string, secretKeys: ReadonlySet<string> = new Set()): PublicEnvironmentEntry[] {
  const result: PublicEnvironmentEntry[] = []
  const seen = new Set<string>()
  for (const [index, rawLine] of text.split(/\r?\n/).entries()) {
    const line = index + 1
    if (!rawLine.trim()) continue
    const separator = rawLine.indexOf('=')
    if (separator < 0) fail(line, 'ENV_TEXT_MISSING_SEPARATOR', `第 ${line} 行缺少 =`)
    const key = rawLine.slice(0, separator).trim()
    const value = rawLine.slice(separator + 1)
    if (!key) fail(line, 'ENV_KEY_REQUIRED', `第 ${line} 行的变量名不能为空`)
    if (!VALID_KEY.test(key)) fail(line, 'ENV_KEY_INVALID', `第 ${line} 行的变量名格式无效`)
    if (seen.has(key)) fail(line, 'ENV_DUPLICATE', `第 ${line} 行的变量名重复`)
    if (secretKeys.has(key)) fail(line, 'ENV_SECRET_CONFLICT', `第 ${line} 行与已保存的 Secret 重名`)
    seen.add(key)
    result.push({ key, value })
  }
  return result
}

export function serializeEnvironmentText(entries: PublicEnvironmentEntry[]): string {
  return entries.map(({ key, value }) => `${key}=${value}`).join('\n')
}
