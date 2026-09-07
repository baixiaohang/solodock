// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import { buildManagedFileProjection, managedFileRowsFromDraft } from '../lib/managedFileRows'
import userEvent from '@testing-library/user-event'
import type { DraftResponse } from '../lib/types'
import { afterEach, describe, expect, it } from 'vitest'

import ManagedFileEditor from './ManagedFileEditor.svelte'

afterEach(cleanup)

describe('ManagedFileEditor', () => {
  it('把嵌套目标冲突定位到服务端指定的可见行', () => {
    render(ManagedFileEditor, {
      rows: [
        {
          logicalName: 'config', targetPath: '/etc/app', sensitive: false,
          originalLogicalName: null, originalTargetPath: null, originalSensitive: false,
          storedSecret: false, removed: false, value: 'root',
        },
        {
          logicalName: 'settings', targetPath: '/etc/app/config.json', sensitive: false,
          originalLogicalName: null, originalTargetPath: null, originalSensitive: false,
          storedSecret: false, removed: false, value: 'nested',
        },
      ],
      issues: [{ path: 'files[1].target_path', code: 'FILE_TARGET_CONFLICT', message: '文件目标不能嵌套' }],
    })

    const paths = screen.getAllByLabelText('容器路径')
    expect(paths[0].getAttribute('aria-invalid')).toBeNull()
    expect(paths[1].getAttribute('aria-invalid')).toBe('true')
    expect(screen.getByText('文件目标不能嵌套')).toBeTruthy()
  })
})


it('preserves public multiline content after projection and readback while keeping stored secrets hidden', async () => {
  const yaml = 'key:\n  value: true\n\n'
  const draft = { files: [ { logical_name: 'config', target_path: '/config', sensitive: false, content: yaml },
    { logical_name: 'key', target_path: '/key', sensitive: true } ] } as DraftResponse
  const rows = managedFileRowsFromDraft(draft)
  render(ManagedFileEditor, { rows })
  const fields = screen.getAllByLabelText('内容')
  expect(fields[0]).toHaveProperty('value', yaml)
  expect(fields[1]).toHaveProperty('value', '')
  expect(buildManagedFileProjection(rows).files[1]).toMatchObject({ operation: 'keep' })
  const user = userEvent.setup()
  await user.click(fields[0]); await user.clear(fields[0]); await user.paste(yaml + 'end\n')
  const saved = buildManagedFileProjection(rows).files
  const reloaded = managedFileRowsFromDraft({ files: saved } as DraftResponse)
  expect(reloaded[0].value).toBe(yaml + 'end\n')
})
