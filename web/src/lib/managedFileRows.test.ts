import { describe, expect, it } from 'vitest'
import { buildManagedFiles, managedFileRowsFromDraft } from './managedFileRows'

function rows() {
  return managedFileRowsFromDraft({
    files: [
      { logical_name: 'public', target_path: '/etc/public', sensitive: false, readonly: true, content: 'visible' },
      { logical_name: 'secret-a', target_path: '/run/a', sensitive: true, readonly: true },
      { logical_name: 'secret-b', target_path: '/run/b', sensitive: true, readonly: true },
    ],
  } as never)
}

describe('managed file row projection', () => {
  it('keeps untouched secrets and edits public content without disclosure', () => {
    const input = rows()
    input[0].value = 'changed'
    expect(buildManagedFiles(input)).toEqual([
      { logical_name: 'public', target_path: '/etc/public', sensitive: false, readonly: true, content: 'changed' },
      { logical_name: 'secret-a', target_path: '/run/a', sensitive: true, readonly: true, operation: 'keep' },
      { logical_name: 'secret-b', target_path: '/run/b', sensitive: true, readonly: true, operation: 'keep' },
    ])
  })

  it('lets a renamed secret take over a deleted secret without duplicate operations', () => {
    const input = rows()
    input[0].removed = true
    input[1].logicalName = 'secret-b'; input[1].targetPath = '/run/replacement'; input[1].value = 'new-b'
    input[2].removed = true
    expect(buildManagedFiles(input)).toEqual([
      { logical_name: 'secret-a', target_path: '/run/a', sensitive: true, readonly: true, operation: 'delete' },
      { logical_name: 'secret-b', target_path: '/run/replacement', sensitive: true, readonly: true, operation: 'replace', value: 'new-b' },
    ])
  })

  it('projects secret-to-public conversion and requires explicit replacement content', () => {
    const converted = rows(); converted[1].sensitive = false; converted[1].value = 'now-public'
    expect(buildManagedFiles(converted)).toEqual([
      { logical_name: 'public', target_path: '/etc/public', sensitive: false, readonly: true, content: 'visible' },
      { logical_name: 'secret-a', target_path: '/run/a', sensitive: false, readonly: true, content: 'now-public' },
      { logical_name: 'secret-a', target_path: '/run/a', sensitive: true, readonly: true, operation: 'delete' },
      { logical_name: 'secret-b', target_path: '/run/b', sensitive: true, readonly: true, operation: 'keep' },
    ])
    const renamed = rows(); renamed[1].logicalName = 'renamed'
    expect(() => buildManagedFiles(renamed)).toThrow('replacement')
    const missing = rows(); missing[1].sensitive = false
    expect(() => buildManagedFiles(missing)).toThrow('replacement')
  })
})
