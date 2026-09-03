import type { DraftInput, DraftResponse } from './types'
import { FormValidationError } from './formErrors'
import type { PublicEnvironmentEntry } from './environmentText'
import { localized } from './i18n'

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

export function emptySecretEnvironmentRow(): EnvironmentRow {
  return { ...emptyEnvironmentRow(), sensitive: true }
}

export function publicEnvironmentEntries(rows: EnvironmentRow[]): PublicEnvironmentEntry[] {
  return rows
    .filter((row) => !row.removed && !row.sensitive)
    .map((row) => ({ key: row.key, value: row.value }))
}

export function replacePublicEnvironmentRows(rows: EnvironmentRow[], entries: PublicEnvironmentEntry[]): EnvironmentRow[] {
  const publicRows = rows.filter((row) => !row.removed && !row.sensitive)
  const retainedSecrets = rows.filter((row) => row.sensitive || (row.removed && row.originalSensitive))
  const projected = entries.map(({ key, value }, index) => {
    const existing = publicRows[index]
    return existing
      ? { ...existing, key, value, sensitive: false, removed: false }
      : {
          id: rowId(), key, value, sensitive: false,
          originalKey: key, originalSensitive: false, storedSecret: false, removed: false,
        }
  })
  const removedSecretConversions = publicRows
    .slice(entries.length)
    .filter((row) => row.originalSensitive)
    .map((row) => ({ ...row, removed: true }))
  return [...projected, ...retainedSecrets, ...removedSecretConversions]
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

export interface EnvironmentProjection {
  environment: DraftInput['environment']
  secretRequestRowIndexes: number[]
}

export function buildEnvironmentProjection(rows: EnvironmentRow[]): EnvironmentProjection {
  const active = rows.filter((row) => !row.removed)
  const visibleSecretRows = active.filter((row) => row.sensitive)
  const finalByKey = new Map<string, EnvironmentRow>()
  let publicIndex = 0
  let secretIndex = 0
  for (const row of active) {
    row.key = row.key.trim()
    const index = row.sensitive ? secretIndex++ : publicIndex++
    const path = row.sensitive ? `environment.secrets[${index}].key` : `environment.public[${index}].key`
    if (!row.key) throw new FormValidationError([{ path, code: 'ENV_KEY_REQUIRED', message: localized('Variable name is required') }])
    if (finalByKey.has(row.key)) throw new FormValidationError([{ path, code: 'ENV_DUPLICATE', message: localized('Variable names must be unique') }])
    finalByKey.set(row.key, row)
    if (row.originalSensitive && !row.sensitive && !row.value) {
      throw new FormValidationError([{ path: `environment.public[${index}].value`, code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter a new value when converting a secret to a public variable') }])
    }
  }

  const publicEntries: DraftInput['environment']['public'] = active
    .filter((row) => !row.sensitive)
    .map((row) => ({ key: row.key, value: row.value }))
  const secretOperations: DraftInput['environment']['secrets'] = []
  const secretRequestRowIndexes: number[] = []
  const originalSecretKeys = new Set(
    rows
      .filter((row) => row.originalSensitive && row.originalKey !== null)
      .map((row) => row.originalKey!),
  )
  for (const key of originalSecretKeys) {
    const finalRow = finalByKey.get(key)
    if (!finalRow || !finalRow.sensitive) {
      secretOperations.push({ key, operation: 'delete' })
      secretRequestRowIndexes.push(-1)
      continue
    }
    const canKeep = finalRow.originalSensitive
      && finalRow.originalKey === key
      && finalRow.storedSecret
      && !finalRow.value
    if (canKeep) secretOperations.push({ key, operation: 'keep' })
    else {
      if (!finalRow.value) throw new FormValidationError([{ path: 'environment.secrets', code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter a new value when renaming a secret or changing its type') }])
      secretOperations.push({ key, operation: 'replace', value: finalRow.value })
    }
    secretRequestRowIndexes.push(visibleSecretRows.indexOf(finalRow))
  }
  for (const row of active) {
    if (!row.sensitive || originalSecretKeys.has(row.key)) continue
    if (!row.value) throw new FormValidationError([{ path: 'environment.secrets', code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter a value for a new secret') }])
    secretOperations.push({ key: row.key, operation: 'replace', value: row.value })
    secretRequestRowIndexes.push(visibleSecretRows.indexOf(row))
  }
  return {
    environment: { public: publicEntries, secrets: secretOperations },
    secretRequestRowIndexes,
  }
}

export function buildEnvironment(rows: EnvironmentRow[]): DraftInput['environment'] {
  return buildEnvironmentProjection(rows).environment
}

export function clearSensitiveEnvironmentValues(rows: EnvironmentRow[]): void {
  for (const row of rows) if (row.sensitive || row.originalSensitive) row.value = ''
}
