export interface EnvEntry { key: string; value: string }

const KEY = /^[A-Za-z_][A-Za-z0-9_]*$/
const INTERPOLATION = /\$(?:\{|\(|[A-Za-z_])|`/

export function parseDotenv(input: string): EnvEntry[] {
  const entries: EnvEntry[] = []
  const seen = new Set<string>()
  for (const source of input.split(/\r?\n/)) {
    let line = source.trim()
    if (!line || line.startsWith('#')) continue
    if (line.startsWith('export ')) line = line.slice(7).trimStart()
    const separator = line.indexOf('=')
    if (separator < 1) throw new Error('malformed dotenv entry')
    const key = line.slice(0, separator).trim()
    if (!KEY.test(key) || seen.has(key)) throw new Error('invalid or duplicate dotenv key')
    const value = parseValue(line.slice(separator + 1).trim())
    if (INTERPOLATION.test(value) || value.includes('\0') || value.includes('\n') || value.includes('\r')) {
      throw new Error('dotenv interpolation or multiline value is not supported')
    }
    seen.add(key)
    entries.push({ key, value })
  }
  return entries.sort((left, right) => left.key.localeCompare(right.key))
}

function parseValue(raw: string): string {
  if (!raw.startsWith('"') && !raw.startsWith("'")) {
    if (raw.includes('\\')) throw new Error('unquoted dotenv escapes are not supported')
    if (/\s+#/.test(raw)) return raw.replace(/\s+#.*$/, '').trimEnd()
    return raw
  }
  const quote = raw[0]
  if (raw.length < 2 || raw.at(-1) !== quote) throw new Error('malformed quoted dotenv value')
  const body = raw.slice(1, -1)
  let output = ''
  for (let index = 0; index < body.length; index += 1) {
    const character = body[index]
    if (character === quote) throw new Error('unescaped quote in dotenv value')
    if (character !== '\\') { output += character; continue }
    const escaped = body[++index]
    if (escaped === undefined || !['\\', '"', "'", '#', '='].includes(escaped)) {
      throw new Error('invalid dotenv escape')
    }
    output += escaped
  }
  return output
}

export function serializeDotenv(entries: EnvEntry[]): string {
  return [...entries]
    .sort((left, right) => left.key.localeCompare(right.key))
    .map(({ key, value }) => `${key}=${quote(value)}`)
    .join('\n')
}

function quote(value: string): string {
  if (/^[A-Za-z0-9_./:@+-]*$/.test(value)) return value
  return `"${value.replaceAll('\\', '\\\\').replaceAll('"', '\\"')}"`
}
