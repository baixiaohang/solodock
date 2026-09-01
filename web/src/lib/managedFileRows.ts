import type { DraftInput, DraftResponse } from './types'

export interface ManagedFileRow {
  logicalName: string
  targetPath: string
  sensitive: boolean
  originalLogicalName: string | null
  originalTargetPath: string | null
  originalSensitive: boolean
  storedSecret: boolean
  removed: boolean
  value: string
}

export function emptyManagedFileRow(): ManagedFileRow {
  return {
    logicalName: '', targetPath: '', sensitive: false,
    originalLogicalName: null, originalTargetPath: null, originalSensitive: false, storedSecret: false,
    removed: false, value: '',
  }
}

export function managedFileRowsFromDraft(draft: DraftResponse): ManagedFileRow[] {
  return draft.files.map((file) => ({
    logicalName: file.logical_name,
    targetPath: file.target_path,
    sensitive: file.sensitive,
    originalLogicalName: file.logical_name,
    originalTargetPath: file.target_path,
    originalSensitive: file.sensitive,
    storedSecret: file.sensitive,
    removed: false,
    value: file.content ?? '',
  }))
}

export function buildManagedFiles(rows: ManagedFileRow[]): DraftInput['files'] {
  const active = rows.filter((row) => !row.removed)
  const finalByName = new Map<string, ManagedFileRow>()
  for (const row of active) {
    row.logicalName = row.logicalName.trim()
    row.targetPath = row.targetPath.trim()
    if (!row.logicalName || !row.targetPath) throw new Error('managed file name and target are required')
    if (finalByName.has(row.logicalName)) throw new Error('duplicate managed file name')
    finalByName.set(row.logicalName, row)
    if (row.originalSensitive && !row.sensitive && !row.value) {
      throw new Error('secret to public conversion requires replacement content')
    }
  }

  const result: DraftInput['files'] = active
    .filter((row) => !row.sensitive)
    .map((row) => ({
      logical_name: row.logicalName,
      target_path: row.targetPath,
      sensitive: false,
      readonly: true,
      content: row.value,
    }))
  const originalSecretNames = new Set(
    rows
      .filter((row) => row.originalSensitive && row.originalLogicalName !== null)
      .map((row) => row.originalLogicalName!),
  )
  for (const name of originalSecretNames) {
    const finalRow = finalByName.get(name)
    if (!finalRow || !finalRow.sensitive) {
      const original = rows.find((row) => row.originalSensitive && row.originalLogicalName === name)!
      result.push({ logical_name: name, target_path: original.originalTargetPath!, sensitive: true, readonly: true, operation: 'delete' })
      continue
    }
    const canKeep = finalRow.originalSensitive
      && finalRow.originalLogicalName === name
      && finalRow.storedSecret
      && !finalRow.value
    if (canKeep) result.push({ logical_name: name, target_path: finalRow.targetPath, sensitive: true, readonly: true, operation: 'keep' })
    else {
      if (!finalRow.value) throw new Error('secret replacement content required')
      result.push({ logical_name: name, target_path: finalRow.targetPath, sensitive: true, readonly: true, operation: 'replace', value: finalRow.value })
    }
  }
  for (const row of active) {
    if (!row.sensitive || originalSecretNames.has(row.logicalName)) continue
    if (!row.value) throw new Error('secret replacement content required')
    result.push({ logical_name: row.logicalName, target_path: row.targetPath, sensitive: true, readonly: true, operation: 'replace', value: row.value })
  }
  return result
}
