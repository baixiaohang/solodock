use std::sync::{Arc, RwLock};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroize;

use super::models::{LogEvent, LogStreamKind};

const MAX_RAW_LINE: usize = 64 * 1024;
const MAX_MESSAGE: usize = 16 * 1024;
const OMITTED: &[u8] = b"[line omitted: too long]";
const REDACTED: &[u8] = b"[REDACTED]";
const MAX_REDACTED_BYTES: usize = 64 * 1024;

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
    patterns: Arc<RwLock<PatternStore>>,
}

#[derive(Default)]
struct PatternStore(Vec<Vec<u8>>);

impl Drop for PatternStore {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SecretRedactor {
    pub fn new(provider: &dyn SecretProvider) -> Self {
        Self {
            patterns: Arc::new(RwLock::new(PatternStore(normalize_patterns(
                provider.known_secrets(),
            )))),
        }
    }

    pub fn replace(&self, patterns: Vec<Vec<u8>>) {
        let patterns = normalize_patterns(patterns);
        let mut current = self
            .patterns
            .write()
            .expect("redactor lock is not poisoned");
        current.0.zeroize();
        current.0 = patterns;
    }

    pub fn extend(&self, patterns: impl IntoIterator<Item = Vec<u8>>) {
        let mut current = self
            .patterns
            .write()
            .expect("redactor lock is not poisoned");
        let mut combined = std::mem::take(&mut current.0);
        combined.extend(patterns);
        current.0 = normalize_patterns(combined);
    }

    /// Returns an operation-local redactor without publishing draft secrets to
    /// the process-wide active/draft secret set.
    pub fn with_additional(&self, patterns: impl IntoIterator<Item = Vec<u8>>) -> Self {
        let mut combined = self
            .patterns
            .read()
            .expect("redactor lock is not poisoned")
            .0
            .clone();
        combined.extend(patterns);
        Self {
            patterns: Arc::new(RwLock::new(PatternStore(normalize_patterns(combined)))),
        }
    }

    pub fn redact(&self, input: &[u8]) -> Vec<u8> {
        let patterns = self.patterns.read().expect("redactor lock is not poisoned");
        redact_once(input, &patterns.0, MAX_REDACTED_BYTES)
    }
}

fn normalize_patterns(patterns: impl IntoIterator<Item = Vec<u8>>) -> Vec<Vec<u8>> {
    let mut normalized = Vec::new();
    for mut secret in patterns {
        if secret.is_empty() {
            secret.zeroize();
            continue;
        }
        for raw_line in secret.split(|byte| *byte == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if !line.is_empty() && line != secret {
                normalized.push(line.to_vec());
            }
        }
        normalized.push(secret);
    }
    normalized.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));

    let mut unique: Vec<Vec<u8>> = Vec::with_capacity(normalized.len());
    for mut pattern in normalized {
        if unique.last().is_some_and(|previous| previous == &pattern) {
            pattern.zeroize();
        } else {
            unique.push(pattern);
        }
    }
    unique
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

fn redact_once(input: &[u8], patterns: &[Vec<u8>], limit: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len().min(limit));
    let mut offset = 0;
    while offset < input.len() && result.len() < limit {
        if let Some(pattern) = patterns
            .iter()
            .find(|pattern| input[offset..].starts_with(pattern.as_slice()))
        {
            let remaining = limit - result.len();
            result.extend_from_slice(&REDACTED[..REDACTED.len().min(remaining)]);
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

    #[test]
    fn redaction_is_single_pass_and_output_is_bounded() {
        let redactor = SecretRedactor::new(&Secrets(vec![
            b"x".to_vec(),
            b"xx".to_vec(),
            b"REDACTED".to_vec(),
        ]));
        assert_eq!(redactor.redact(b"xx"), REDACTED);
        assert_eq!(redactor.redact(b"[REDACTED]"), b"[[REDACTED]]");
        let output = redactor.redact(&vec![b'x'; MAX_RAW_LINE]);
        assert!(output.len() <= MAX_REDACTED_BYTES);
    }

    #[test]
    fn operation_local_patterns_do_not_pollute_global_set() {
        let global = SecretRedactor::new(&EmptySecretProvider);
        let local = global.with_additional([b"preview-only".to_vec()]);
        assert_eq!(local.redact(b"preview-only"), REDACTED);
        assert_eq!(global.redact(b"preview-only"), b"preview-only");
    }

    #[test]
    fn normalizes_trailing_newlines_crlf_and_multiline_secrets() {
        let redactor = SecretRedactor::new(&Secrets(vec![
            b"token-with-lf\n".to_vec(),
            b"token-with-crlf\r\n".to_vec(),
            b"-----BEGIN KEY-----\nprivate-material\n-----END KEY-----\n".to_vec(),
            b"\n\r\n".to_vec(),
        ]));
        for exposed_line in [
            b"token-with-lf".as_slice(),
            b"token-with-crlf".as_slice(),
            b"-----BEGIN KEY-----".as_slice(),
            b"private-material".as_slice(),
            b"-----END KEY-----".as_slice(),
        ] {
            assert_eq!(redactor.redact(exposed_line), REDACTED);
        }
        assert_eq!(redactor.redact(b"unrelated"), b"unrelated");
    }

    #[test]
    fn every_update_path_uses_the_same_pattern_normalization() {
        let redactor = SecretRedactor::new(&Secrets(vec![b"initial\nline".to_vec()]));
        assert_eq!(redactor.redact(b"line"), REDACTED);

        redactor.replace(vec![b"replacement\r\nline".to_vec()]);
        assert_eq!(redactor.redact(b"initial"), b"initial");
        assert_eq!(redactor.redact(b"replacement"), REDACTED);
        assert_eq!(redactor.redact(b"line"), REDACTED);

        redactor.extend([b"extended\nsegment".to_vec()]);
        assert_eq!(redactor.redact(b"extended"), REDACTED);
        assert_eq!(redactor.redact(b"segment"), REDACTED);

        let local = redactor.with_additional([b"local\r\nsecret".to_vec()]);
        assert_eq!(local.redact(b"local"), REDACTED);
        assert_eq!(local.redact(b"secret"), REDACTED);
        assert_eq!(redactor.redact(b"local"), b"local");
    }

    #[test]
    fn multiline_secret_lines_are_redacted_across_log_chunks() {
        let redactor =
            SecretRedactor::new(&Secrets(vec![b"first-line\r\nsecond-line\r\n".to_vec()]));
        let mut framer = LogFramer::new(redactor);
        assert!(
            framer
                .push(LogStreamKind::Stdout, b"2026-08-28T00:00:00Z first-")
                .is_empty()
        );
        let first = framer.push(LogStreamKind::Stdout, b"line\nsecond-");
        assert_eq!(first[0].message, "[REDACTED]");
        let second = framer.push(LogStreamKind::Stdout, b"line\n");
        assert_eq!(second[0].message, "[REDACTED]");
    }
}
