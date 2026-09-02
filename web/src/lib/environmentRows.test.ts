import { describe, expect, it, vi } from 'vitest'
import { buildEnvironment, buildEnvironmentProjection, emptyEnvironmentRow, environmentRowsFromDraft, replacePublicEnvironmentRows } from './environmentRows'

vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000001' })

describe('environment row projection', () => {
  it('keeps stored secrets without exposing a value and replaces public values directly', () => {
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'LOG_LEVEL', value: 'info' }],
      secret_keys: ['TOKEN'],
    })
    rows[0].value = 'debug'
    expect(buildEnvironment(rows)).toEqual({
      public: [{ key: 'LOG_LEVEL', value: 'debug' }],
      secrets: [{ key: 'TOKEN', operation: 'keep' }],
    })
  })

  it('covers replace, delete, rename and public-secret conversion deterministically', () => {
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'visible' }],
      secret_keys: ['OLD', 'REMOVE'],
    })
    rows[0].sensitive = true
    rows[1].key = 'NEW'; rows[1].value = 'replacement'
    rows[2].removed = true
    expect(buildEnvironment(rows)).toEqual({
      public: [],
      secrets: [
        { key: 'OLD', operation: 'delete' },
        { key: 'REMOVE', operation: 'delete' },
        { key: 'PUBLIC', operation: 'replace', value: 'visible' },
        { key: 'NEW', operation: 'replace', value: 'replacement' },
      ],
    })
  })

  it('rejects duplicate keys and secret-to-public without a new value', () => {
    const duplicate = [emptyEnvironmentRow(), emptyEnvironmentRow()]
    duplicate[0].key = 'A'; duplicate[1].key = 'A'
    expect(() => buildEnvironment(duplicate)).toThrow('变量名不能重复')
    const secret = environmentRowsFromDraft({ public_environment: [], secret_keys: ['TOKEN'] })
    secret[0].sensitive = false
    expect(() => buildEnvironment(secret)).toThrow('必须输入新值')
  })

  it('preserves secret conversion identity while batch-editing public rows', () => {
    const rows = environmentRowsFromDraft({ public_environment: [], secret_keys: ['TOKEN'] })
    rows[0].sensitive = false
    rows[0].value = 'visible'
    const projected = replacePublicEnvironmentRows(rows, [{ key: 'RENAMED', value: 'visible' }])

    expect(projected).toHaveLength(1)
    expect(projected[0]).toMatchObject({ key: 'RENAMED', originalKey: 'TOKEN', originalSensitive: true })
    expect(buildEnvironment(projected)).toEqual({
      public: [{ key: 'RENAMED', value: 'visible' }],
      secrets: [{ key: 'TOKEN', operation: 'delete' }],
    })

    const removed = replacePublicEnvironmentRows(projected, [])
    expect(buildEnvironment(removed)).toEqual({ public: [], secrets: [{ key: 'TOKEN', operation: 'delete' }] })
  })

  it('lets a renamed secret take over a deleted secret key without duplicate operations', () => {
    const rows = environmentRowsFromDraft({ public_environment: [], secret_keys: ['A', 'B'] })
    rows[0].key = 'B'; rows[0].value = 'replacement-for-b'
    rows[1].removed = true
    expect(buildEnvironment(rows)).toEqual({
      public: [],
      secrets: [
        { key: 'A', operation: 'delete' },
        { key: 'B', operation: 'replace', value: 'replacement-for-b' },
      ],
    })
  })

  it('supports public-secret takeover, public takeover and secret rename cycles', () => {
    const takeover = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'new-b' }],
      secret_keys: ['B'],
    })
    takeover[0].key = 'B'; takeover[0].sensitive = true
    takeover[1].removed = true
    expect(buildEnvironment(takeover)).toEqual({
      public: [],
      secrets: [{ key: 'B', operation: 'replace', value: 'new-b' }],
    })

    const publicTakeover = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'visible-b' }],
      secret_keys: ['B'],
    })
    publicTakeover[0].key = 'B'
    publicTakeover[1].removed = true
    expect(buildEnvironment(publicTakeover)).toEqual({
      public: [{ key: 'B', value: 'visible-b' }],
      secrets: [{ key: 'B', operation: 'delete' }],
    })

    const cycle = environmentRowsFromDraft({ public_environment: [], secret_keys: ['A', 'B'] })
    cycle[0].key = 'B'; cycle[0].value = 'from-a'
    cycle[1].key = 'A'; cycle[1].value = 'from-b'
    expect(buildEnvironment(cycle)).toEqual({
      public: [],
      secrets: [
        { key: 'A', operation: 'replace', value: 'from-b' },
        { key: 'B', operation: 'replace', value: 'from-a' },
      ],
    })
  })

  it('maps Secret request operations back to visible Secret rows', () => {
    const rows = environmentRowsFromDraft({ public_environment: [], secret_keys: ['OLD'] })
    rows[0].removed = true
    const replacement = emptyEnvironmentRow()
    replacement.key = 'NEW'
    replacement.value = 'replacement'
    replacement.sensitive = true
    rows.push(replacement)

    expect(buildEnvironmentProjection(rows)).toEqual({
      environment: {
        public: [],
        secrets: [
          { key: 'OLD', operation: 'delete' },
          { key: 'NEW', operation: 'replace', value: 'replacement' },
        ],
      },
      secretRequestRowIndexes: [-1, 0],
    })
  })
})
