import { describe, expect, it } from 'vitest'
import { parseDotenv, serializeDotenv } from './dotenv'

describe('finite dotenv grammar', () => {
  it('supports comments, export, quotes and deterministic round trips', () => {
    const parsed = parseDotenv(`# comment\nexport B="two words"\nA='one\\#value'`)
    expect(parsed).toEqual([{ key: 'A', value: 'one#value' }, { key: 'B', value: 'two words' }])
    expect(parseDotenv(serializeDotenv(parsed))).toEqual(parsed)
  })

  it.each(['A=1\nA=2', 'A=${B}', 'A=$(command)', 'A=`command`', 'A="unterminated', 'A="a"b"', "A='a'b'", 'A=bad\\q'])('rejects unsafe input %s', (value) => {
    expect(() => parseDotenv(value)).toThrow()
  })
})
