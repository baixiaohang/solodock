use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::security::secret::SecretValue;

pub const MAX_BODY_BYTES: usize = 1024;
pub const TIMESTAMP_WINDOW_SECONDS: i64 = 300;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebhookPayload {
    pub event: WebhookEvent,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub enum WebhookEvent {
    #[serde(rename = "registry.push")]
    RegistryPush,
}

pub struct VerifiedHeaders {
    pub timestamp: i64,
    pub nonce: String,
    pub nonce_sha256: [u8; 32],
    pub signature: [u8; 32],
}

pub fn parse_headers(
    timestamp: &str,
    nonce: &str,
    signature: &str,
) -> Result<VerifiedHeaders, ProtocolError> {
    if timestamp.is_empty()
        || (timestamp.len() > 1 && timestamp.starts_with('0'))
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(ProtocolError::Header);
    }
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| ProtocolError::Header)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| ProtocolError::Header)?;
    if decoded.len() != 16 || URL_SAFE_NO_PAD.encode(&decoded) != nonce {
        return Err(ProtocolError::Header);
    }
    let Some(hex) = signature.strip_prefix("v1=") else {
        return Err(ProtocolError::Header);
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ProtocolError::Header);
    }
    let mut signature_bytes = [0u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        signature_bytes[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Ok(VerifiedHeaders {
        timestamp,
        nonce: nonce.to_owned(),
        nonce_sha256: Sha256::digest(nonce.as_bytes()).into(),
        signature: signature_bytes,
    })
}

pub fn verify(
    app_id: Uuid,
    raw_body: &[u8],
    headers: &VerifiedHeaders,
    secret: &SecretValue,
    now: OffsetDateTime,
) -> Result<(), ProtocolError> {
    if (now.unix_timestamp() - headers.timestamp).abs() > TIMESTAMP_WINDOW_SECONDS {
        return Err(ProtocolError::Timestamp);
    }
    let decoded = decode_secret(secret)?;
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&decoded) != secret.expose() {
        return Err(ProtocolError::Secret);
    }
    let input = signing_input(app_id, raw_body, headers.timestamp, &headers.nonce);
    let mut mac = Hmac::<Sha256>::new_from_slice(&decoded).map_err(|_| ProtocolError::Secret)?;
    mac.update(input.as_bytes());
    let expected: [u8; 32] = mac.finalize().into_bytes().into();
    if !bool::from(expected.ct_eq(&headers.signature)) {
        return Err(ProtocolError::Signature);
    }
    Ok(())
}

pub fn signing_input(app_id: Uuid, body: &[u8], timestamp: i64, nonce: &str) -> String {
    format!(
        "solodock-webhook-v1\n{timestamp}\n{nonce}\nPOST\n/hooks/v1/apps/{app_id}/registry\n{:x}",
        Sha256::digest(body)
    )
}

pub fn validate_secret(secret: &SecretValue) -> Result<(), ProtocolError> {
    decode_secret(secret).map(|_| ())
}

pub(crate) fn decode_secret(secret: &SecretValue) -> Result<Zeroizing<Vec<u8>>, ProtocolError> {
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(secret.expose())
            .map_err(|_| ProtocolError::Secret)?,
    );
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&decoded) != secret.expose() {
        return Err(ProtocolError::Secret);
    }
    Ok(decoded)
}

fn hex_value(value: u8) -> Result<u8, ProtocolError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ProtocolError::Header),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid webhook header")]
    Header,
    #[error("webhook timestamp is outside the accepted window")]
    Timestamp,
    #[error("invalid webhook signature")]
    Signature,
    #[error("invalid webhook secret")]
    Secret,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_signature_round_trip_and_malleability_rejection() {
        let app = Uuid::parse_str("19f20844-d7af-4b1a-861b-c7f80dc3272d").unwrap();
        let body = br#"{"event":"registry.push"}"#;
        let nonce = "AAECAwQFBgcICQoLDA0ODw";
        let secret = SecretValue::new("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into());
        let input = signing_input(app, body, 1_800_000_000, nonce);
        let key = URL_SAFE_NO_PAD.decode(secret.expose()).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        mac.update(input.as_bytes());
        let signature = format!("v1={:x}", mac.finalize().into_bytes());
        let headers = parse_headers("1800000000", nonce, &signature).unwrap();
        verify(
            app,
            body,
            &headers,
            &secret,
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        )
        .unwrap();
        assert!(
            verify(
                app,
                body,
                &headers,
                &secret,
                OffsetDateTime::from_unix_timestamp(1_800_000_301).unwrap(),
            )
            .is_err()
        );
        assert!(
            verify(
                app,
                body,
                &headers,
                &secret,
                OffsetDateTime::from_unix_timestamp(1_799_999_699).unwrap(),
            )
            .is_err()
        );
        assert!(parse_headers("01800000000", nonce, &signature).is_err());
        assert!(parse_headers("1800000000", &format!("{nonce}="), &signature).is_err());
        assert!(parse_headers("1800000000", nonce, &signature.to_uppercase()).is_err());
    }
}
