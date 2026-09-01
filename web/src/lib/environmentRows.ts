import type { DraftInput, DraftResponse } from './types'

export interface EnvironmentRow {
  id: string
  key: string
  value: string
  sensitive: boolean
  originalKey: string | null
  originalSensitive: boolean
  storedSecret: boolean
  removed: boolean
}

function rowId(): string {
  return crypto.randomUUID()
}

export function emptyEnvironmentRow(): EnvironmentRow {
  return {
    id: rowId(), key: '', value: '', sensitive: false,
    originalKey: null, originalSensitive: false, storedSecret: false, removed: false,
  }
}

export function environmentRowsFromDraft(draft: Pick<DraftResponse, 'public_environment' | 'secret_keys'>): EnvironmentRow[] {
  return [
    ...draft.public_environment.map(({ key, value }) => ({
      id: rowId(), key, value, sensitive: false,
      originalKey: key, originalSensitive: false, storedSecret: false, removed: false,
    })),
    ...draft.secret_keys.map((key) => ({
      id: rowId(), key, value: '', sensitive: true,
      originalKey: key, originalSensitive: true, storedSecret: true, removed: false,
    })),
  ]
}

export function buildEnvironment(rows: EnvironmentRow[]): DraftInput['environment'] {
  const active = rows.filter((row) => !row.removed)
  const finalByKey = new Map<string, EnvironmentRow>()
  for (const row of active) {
    row.key = row.key.trim()
    if (!row.key) throw new Error('environment key required')
    if (finalByKey.has(row.key)) throw new Error('duplicate environment key')
    finalByKey.set(row.key, row)
    if (row.originalSensitive && !row.sensitive && !row.value) {
      throw new Error('secret to public conversion requires a replacement value')
    }
  }

  const publicEntries: DraftInput['environment']['public'] = active
    .filter((row) => !row.sensitive)
    .map((row) => ({ key: row.key, value: row.value }))
  const secretOperations: DraftInput['environment']['secrets'] = []
  const originalSecretKeys = new Set(
    rows
      .filter((row) => row.originalSensitive && row.originalKey !== null)
      .map((row) => row.originalKey!),
  )
  for (const key of originalSecretKeys) {
    const finalRow = finalByKey.get(key)
    if (!finalRow || !finalRow.sensitive) {
      secretOperations.push({ key, operation: 'delete' })
      continue
    }
    const canKeep = finalRow.originalSensitive
      && finalRow.originalKey === key
      && finalRow.storedSecret
      && !finalRow.value
    if (canKeep) secretOperations.push({ key, operation: 'keep' })
    else {
      if (!finalRow.value) throw new Error('secret replacement value required')
      secretOperations.push({ key, operation: 'replace', value: finalRow.value })
    }
  }
  for (const row of active) {
    if (!row.sensitive || originalSecretKeys.has(row.key)) continue
    if (!row.value) throw new Error('secret replacement value required')
    secretOperations.push({ key: row.key, operation: 'replace', value: row.value })
  }
  return { public: publicEntries, secrets: secretOperations }
}

export function clearSensitiveEnvironmentValues(rows: EnvironmentRow[]): void {
  for (const row of rows) if (row.sensitive || row.originalSensitive) row.value = ''
}
