import type { DraftInput, DraftResponse } from './types'
import { FormValidationError } from './formErrors'
import { localized } from './i18n'

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

export interface ManagedFileProjection {
  files: DraftInput['files']
  requestRowIndexes: number[]
}

export function buildManagedFileProjection(rows: ManagedFileRow[]): ManagedFileProjection {
  const active = rows.filter((row) => !row.removed)
  const finalByName = new Map<string, ManagedFileRow>()
  for (const [index, row] of active.entries()) {
    row.logicalName = row.logicalName.trim()
    row.targetPath = row.targetPath.trim()
    if (!row.logicalName) throw new FormValidationError([{ path: `files[${index}].logical_name`, code: 'FILE_NAME_REQUIRED', message: localized('Managed file logical name is required') }])
    if (!row.targetPath) throw new FormValidationError([{ path: `files[${index}].target_path`, code: 'FILE_TARGET_REQUIRED', message: localized('Managed file container path is required') }])
    if (finalByName.has(row.logicalName)) throw new FormValidationError([{ path: `files[${index}].logical_name`, code: 'FILE_TARGET_CONFLICT', message: localized('Managed file logical names must be unique') }])
    finalByName.set(row.logicalName, row)
    if (row.originalSensitive && !row.sensitive && !row.value) {
      throw new FormValidationError([{ path: `files[${index}].content`, code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter new content when converting a secret file to a regular file') }])
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
  const requestRowIndexes = active.flatMap((row, index) => row.sensitive ? [] : [index])
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
      requestRowIndexes.push(finalRow ? active.indexOf(finalRow) : active.indexOf(original))
      continue
    }
    const canKeep = finalRow.originalSensitive
      && finalRow.originalLogicalName === name
      && finalRow.storedSecret
      && !finalRow.value
    if (canKeep) result.push({ logical_name: name, target_path: finalRow.targetPath, sensitive: true, readonly: true, operation: 'keep' })
    else {
      if (!finalRow.value) throw new FormValidationError([{ path: 'files', code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter new content when renaming a secret file or changing its type') }])
      result.push({ logical_name: name, target_path: finalRow.targetPath, sensitive: true, readonly: true, operation: 'replace', value: finalRow.value })
    }
    requestRowIndexes.push(active.indexOf(finalRow))
  }
  for (const row of active) {
    if (!row.sensitive || originalSecretNames.has(row.logicalName)) continue
    if (!row.value) throw new FormValidationError([{ path: 'files', code: 'SECRET_REPLACEMENT_REQUIRED', message: localized('Enter content for a new secret file') }])
    result.push({ logical_name: row.logicalName, target_path: row.targetPath, sensitive: true, readonly: true, operation: 'replace', value: row.value })
    requestRowIndexes.push(active.indexOf(row))
  }
  return { files: result, requestRowIndexes }
}

export function buildManagedFiles(rows: ManagedFileRow[]): DraftInput['files'] {
  return buildManagedFileProjection(rows).files
}
