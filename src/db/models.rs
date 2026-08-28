use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AuditMetadata {
    reason_code: Option<&'static str>,
}

impl AuditMetadata {
    pub fn empty() -> Self {
        Self { reason_code: None }
    }

    pub fn reason(reason_code: &'static str) -> Self {
        Self {
            reason_code: Some(reason_code),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("typed audit metadata is serializable")
    }
}

#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub id: String,
    pub created_at: String,
    pub last_seen_at: String,
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
}

#[derive(Clone, Debug)]
pub struct ThrottleRecord {
    pub window_started_at: Option<String>,
    pub failure_count: i64,
    pub blocked_until: Option<String>,
}
