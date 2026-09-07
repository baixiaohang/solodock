import type { EnvironmentRow } from './environmentRows'
import type { ManagedFileRow } from './managedFileRows'

// Capture after projection has normalized names, before the request can yield.
export function submittedDraftRows(environment: EnvironmentRow[], files: ManagedFileRow[]) {
  return {
    environment: environment.filter((row) => !row.removed).map((row) => ({ ...row })),
    files: files.filter((row) => !row.removed).map((row) => ({ source: row, saved: { ...row } })),
  }
}

export function acceptEnvironmentSave(current: EnvironmentRow[], submitted: EnvironmentRow[]): EnvironmentRow[] {
  const rows = current.map((row) => {
    const saved = submitted.find((saved) => saved.id === row.id)
    return {
      ...row,
      originalKey: saved?.key ?? null,
      originalSensitive: saved?.sensitive ?? false,
      storedSecret: saved?.sensitive ?? false,
      value: saved?.sensitive && row.sensitive && row.key === saved.key && row.value === saved.value ? '' : row.value,
    }
  })
  // A newly submitted Secret may have been removed before its success arrived.
  for (const saved of submitted) {
    if (saved.sensitive && !current.some((row) => row.id === saved.id)) {
      rows.push({ ...saved, value: '', originalKey: saved.key, originalSensitive: true, storedSecret: true, removed: true })
    }
  }
  return rows.filter((row) => !row.removed || row.originalSensitive)
}

export function acceptManagedFileSave(current: ManagedFileRow[], submitted: ReturnType<typeof submittedDraftRows>['files']): ManagedFileRow[] {
  const rows = current.map((row) => {
    const saved = submitted.find((entry) => entry.source === row)?.saved
    return {
      ...row,
      originalLogicalName: saved?.logicalName ?? null,
      originalTargetPath: saved?.targetPath ?? null,
      originalSensitive: saved?.sensitive ?? false,
      storedSecret: saved?.sensitive ?? false,
      value: saved?.sensitive && row.sensitive && row.logicalName === saved.logicalName && row.value === saved.value ? '' : row.value,
    }
  })
  for (const { source, saved } of submitted) {
    if (saved.sensitive && !current.includes(source)) {
      rows.push({ ...saved, value: '', originalLogicalName: saved.logicalName, originalTargetPath: saved.targetPath,
        originalSensitive: true, storedSecret: true, removed: true })
    }
  }
  return rows.filter((row) => !row.removed || row.originalSensitive)
}
