import { describe, expect, it } from 'vitest'
import { acceptEnvironmentSave, acceptManagedFileSave, submittedDraftRows } from './draftSaveState'
import { buildEnvironment, emptySecretEnvironmentRow } from './environmentRows'
import { buildManagedFiles, emptyManagedFileRow } from './managedFileRows'

function inputs() {
  return {
    environment: [{ ...emptySecretEnvironmentRow(), key: 'TOKEN', value: 'submitted' }],
    files: [{ ...emptyManagedFileRow(), logicalName: 'key', targetPath: '/key', sensitive: true, value: 'submitted' }],
  }
}

describe('confirmed draft row baselines', () => {
  it.each(['keep', 'replace', 'delete', 'rename', 'public'] as const)('projects newly saved sensitive entries after concurrent %s', (change) => {
    const { environment, files } = inputs()
    const submitted = submittedDraftRows(environment, files)
    if (change === 'replace') { environment[0].value = 'next'; files[0].value = 'next' }
    if (change === 'rename') { environment[0].key = 'NEXT'; files[0].logicalName = 'next' }
    if (change === 'public') {
      environment[0].sensitive = false; environment[0].value = 'public'
      files[0].sensitive = false; files[0].value = 'public'
    }
    const nextEnvironment = acceptEnvironmentSave(change === 'delete' ? [] : environment, submitted.environment)
    const nextFiles = acceptManagedFileSave(change === 'delete' ? [] : files, submitted.files)
    const env = buildEnvironment(nextEnvironment)
    const file = buildManagedFiles(nextFiles)
    if (change === 'keep' || change === 'replace') {
      expect(env.secrets).toEqual([{ key: 'TOKEN', operation: change, ...(change === 'replace' ? { value: 'next' } : {}) }])
      expect(file).toEqual([{ logical_name: 'key', target_path: '/key', sensitive: true, readonly: true, operation: change, ...(change === 'replace' ? { value: 'next' } : {}) }])
    } else {
      expect(env.secrets).toContainEqual({ key: 'TOKEN', operation: 'delete' })
      expect(file).toContainEqual({ logical_name: 'key', target_path: '/key', sensitive: true, readonly: true, operation: 'delete' })
      if (change === 'rename') {
        expect(env.secrets).toContainEqual({ key: 'NEXT', operation: 'replace', value: 'submitted' })
        expect(file).toContainEqual({ logical_name: 'next', target_path: '/key', sensitive: true, readonly: true, operation: 'replace', value: 'submitted' })
      }
      if (change === 'public') {
        expect(env.public).toEqual([{ key: 'TOKEN', value: 'public' }])
        expect(file).toContainEqual({ logical_name: 'key', target_path: '/key', sensitive: false, readonly: true, content: 'public' })
      }
    }
  })

  it('forgets confirmed deletion and type conversion baselines on the following save', () => {
    const { environment, files } = inputs()
    let saved = submittedDraftRows(environment, files)
    let env = acceptEnvironmentSave(environment, saved.environment)
    let file = acceptManagedFileSave(files, saved.files)
    env[0].sensitive = false; env[0].value = 'public'
    file[0].sensitive = false; file[0].value = 'public'
    saved = submittedDraftRows(env, file)
    env = acceptEnvironmentSave(env, saved.environment); file = acceptManagedFileSave(file, saved.files)
    env[0].value = ''; file[0].value = ''
    expect(buildEnvironment(env)).toEqual({ public: [{ key: 'TOKEN', value: '' }], secrets: [] })
    expect(buildManagedFiles(file)).toEqual([{ logical_name: 'key', target_path: '/key', sensitive: false, readonly: true, content: '' }])
    env[0].removed = true; file[0].removed = true
    saved = submittedDraftRows(env, file)
    expect(acceptEnvironmentSave(env, saved.environment)).toEqual([])
    expect(acceptManagedFileSave(file, saved.files)).toEqual([])
  })

  it('does not clear an edited file value when its row moves or target changes', () => {
    const { environment, files } = inputs()
    const submitted = submittedDraftRows(environment, files)
    files[0].targetPath = '/next'; files[0].value = 'next'
    const next = acceptManagedFileSave([emptyManagedFileRow(), files[0]].slice(1), submitted.files)
    expect(buildManagedFiles(next)).toEqual([{ logical_name: 'key', target_path: '/next', sensitive: true, readonly: true, operation: 'replace', value: 'next' }])
  })
})
