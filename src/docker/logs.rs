use std::sync::Arc;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::models::{LogEvent, LogStreamKind};

const MAX_RAW_LINE: usize = 64 * 1024;
const MAX_MESSAGE: usize = 16 * 1024;
const OMITTED: &[u8] = b"[line omitted: too long]";
const REDACTED: &[u8] = b"[REDACTED]";

pub trait SecretProvider: Send + Sync {
    fn known_secrets(&self) -> Vec<Vec<u8>>;
}

#[derive(Default)]
pub struct EmptySecretProvider;

impl SecretProvider for EmptySecretProvider {
    fn known_secrets(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

#[derive(Clone)]
pub struct SecretRedactor {
    patterns: Arc<Vec<Vec<u8>>>,
}

impl SecretRedactor {
    pub fn new(provider: &dyn SecretProvider) -> Self {
        let mut patterns: Vec<_> = provider
            .known_secrets()
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        patterns.sort_by_key(|value| std::cmp::Reverse(value.len()));
        patterns.dedup();
        Self {
            patterns: Arc::new(patterns),
        }
    }

    pub fn redact(&self, input: &[u8]) -> Vec<u8> {
        let mut output = input.to_vec();
        for secret in self.patterns.iter() {
            output = replace_bytes(&output, secret, REDACTED);
        }
        output
    }
}

#[derive(Default)]
struct LineState {
    bytes: Vec<u8>,
    oversized: bool,
}

pub struct LogFramer {
    stdout: LineState,
    stderr: LineState,
    redactor: SecretRedactor,
}

impl LogFramer {
    pub fn new(redactor: SecretRedactor) -> Self {
        Self {
            stdout: LineState::default(),
            stderr: LineState::default(),
            redactor,
        }
    }

    pub fn push(&mut self, stream: LogStreamKind, chunk: &[u8]) -> Vec<LogEvent> {
        let state = match stream {
            LogStreamKind::Stdout => &mut self.stdout,
            LogStreamKind::Stderr => &mut self.stderr,
            LogStreamKind::Unknown => return Vec::new(),
        };
        let mut result = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let input = if state.oversized {
                    OMITTED
                } else {
                    &state.bytes
                };
                result.push(format_line(&self.redactor, stream, input, state.oversized));
                state.bytes.clear();
                state.oversized = false;
            } else if !state.oversized {
                state.bytes.push(*byte);
                if state.bytes.len() > MAX_RAW_LINE {
                    state.bytes.clear();
                    state.oversized = true;
                }
            }
        }
        result
    }

    pub fn finish(&mut self) -> Vec<LogEvent> {
        let mut result = Vec::new();
        if let Some(event) = finish_state(&self.redactor, LogStreamKind::Stdout, &mut self.stdout) {
            result.push(event);
        }
        if let Some(event) = finish_state(&self.redactor, LogStreamKind::Stderr, &mut self.stderr) {
            result.push(event);
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogCursor {
    pub unix_nanos: i128,
    pub ordinal: u32,
}

impl LogCursor {
    pub fn encode(self) -> String {
        format!("{}:{}", self.unix_nanos, self.ordinal)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (nanos, ordinal) = value.split_once(':')?;
        Some(Self {
            unix_nanos: nanos.parse().ok()?,
            ordinal: ordinal.parse().ok()?,
        })
    }

    pub fn since_seconds(self) -> i64 {
        self.unix_nanos
            .div_euclid(1_000_000_000)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }
}

fn format_line(
    redactor: &SecretRedactor,
    stream: LogStreamKind,
    raw: &[u8],
    oversized: bool,
) -> LogEvent {
    let now = OffsetDateTime::now_utc();
    let (timestamp, message) = split_timestamp(raw).unwrap_or((now, raw));
    let redacted = if oversized {
        OMITTED.to_vec()
    } else {
        redactor.redact(message)
    };
    let cleaned = clean_text(&String::from_utf8_lossy(&redacted));
    let (message, truncated) = truncate_utf8(&cleaned, MAX_MESSAGE);
    LogEvent {
        timestamp: timestamp
            .format(&Rfc3339)
            .expect("UTC time formats as RFC3339"),
        stream,
        message,
        truncated: oversized || truncated,
    }
}

fn finish_state(
    redactor: &SecretRedactor,
    stream: LogStreamKind,
    state: &mut LineState,
) -> Option<LogEvent> {
    if state.bytes.is_empty() && !state.oversized {
        return None;
    }
    let input = if state.oversized {
        OMITTED
    } else {
        &state.bytes
    };
    let event = format_line(redactor, stream, input, state.oversized);
    state.bytes.clear();
    state.oversized = false;
    Some(event)
}

fn split_timestamp(raw: &[u8]) -> Option<(OffsetDateTime, &[u8])> {
    let separator = raw.iter().position(|byte| *byte == b' ')?;
    let timestamp = std::str::from_utf8(&raw[..separator]).ok()?;
    let parsed = OffsetDateTime::parse(timestamp, &Rfc3339).ok()?;
    Some((parsed, &raw[separator + 1..]))
}

fn replace_bytes(input: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        if input[offset..].starts_with(pattern) {
            result.extend_from_slice(replacement);
            offset += pattern.len();
        } else {
            result.push(input[offset]);
            offset += 1;
        }
    }
    result
}

fn clean_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for value in chars.by_ref() {
                        if value.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(value) = chars.next() {
                        if value == '\u{7}' {
                            break;
                        }
                        if value == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Secrets(Vec<Vec<u8>>);
    impl SecretProvider for Secrets {
        fn known_secrets(&self) -> Vec<Vec<u8>> {
            self.0.clone()
        }
    }

    #[test]
    fn redacts_across_chunks_before_utf8_conversion_and_strips_controls() {
        let redactor = SecretRedactor::new(&Secrets(vec![b"managed-secret".to_vec()]));
        let mut framer = LogFramer::new(redactor);
        assert!(
            framer
                .push(LogStreamKind::Stdout, b"2026-08-28T00:00:00Z managed-")
                .is_empty()
        );
        let lines = framer.push(LogStreamKind::Stdout, b"secret \x1b[31mred\x1b[0m\0\xff\n");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].message.contains("[REDACTED] red"));
        assert!(!lines[0].message.contains("managed-secret"));
        assert!(!lines[0].message.contains('\u{1b}'));
        assert!(!lines[0].message.contains('\0'));
    }

    #[test]
    fn oversized_line_never_leaks_prefix() {
        let mut framer = LogFramer::new(SecretRedactor::new(&EmptySecretProvider));
        let mut input = b"sensitive-prefix".to_vec();
        input.resize(MAX_RAW_LINE + 10, b'x');
        input.push(b'\n');
        let line = framer.push(LogStreamKind::Stderr, &input).remove(0);
        assert_eq!(line.message, "[line omitted: too long]");
        assert!(!line.message.contains("sensitive"));
        assert!(line.truncated);
    }

    #[test]
    fn flushes_final_logical_line_without_newline() {
        let mut framer = LogFramer::new(SecretRedactor::new(&EmptySecretProvider));
        assert!(framer.push(LogStreamKind::Stdout, b"last line").is_empty());
        let lines = framer.finish();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "last line");
    }
}
