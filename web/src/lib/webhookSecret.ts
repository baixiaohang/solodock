export function encodeWebhookSecret(bytes: Uint8Array): string {
  if (bytes.length !== 32) throw new Error('webhook secret must be 32 bytes')
  const encoded = btoa(String.fromCharCode(...bytes))
    .replaceAll('+', '-')
    .replaceAll('/', '_')
    .replace(/=+$/, '')
  bytes.fill(0)
  return encoded
}

