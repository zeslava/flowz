use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WebhookError {
    #[error("missing X-Hub-Signature-256 header")]
    MissingSignature,
    #[error("invalid signature format")]
    InvalidSignatureFormat,
    #[error("signature mismatch")]
    SignatureMismatch,
    #[error("json parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("not a push event")]
    NotPushEvent,
}

#[derive(Debug, Clone)]
pub struct PushEvent {
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub clone_url: String,
}

pub fn verify_signature(secret: &str, body: &[u8], sig_header: &str) -> Result<(), WebhookError> {
    let hex_sig = sig_header
        .strip_prefix("sha256=")
        .ok_or(WebhookError::InvalidSignatureFormat)?;

    let expected = hex::decode(hex_sig).map_err(|_| WebhookError::InvalidSignatureFormat)?;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| WebhookError::SignatureMismatch)
}

pub fn parse_push(body: &[u8]) -> Result<PushEvent, WebhookError> {
    let v: serde_json::Value = serde_json::from_slice(body)?;

    let ref_field = v["ref"].as_str().ok_or(WebhookError::NotPushEvent)?;
    let branch = ref_field
        .strip_prefix("refs/heads/")
        .ok_or(WebhookError::NotPushEvent)?
        .to_string();

    let commit = v["after"]
        .as_str()
        .ok_or(WebhookError::NotPushEvent)?
        .to_string();

    let repo = v["repository"]["full_name"]
        .as_str()
        .ok_or(WebhookError::NotPushEvent)?
        .to_string();

    let clone_url = v["repository"]["clone_url"]
        .as_str()
        .ok_or(WebhookError::NotPushEvent)?
        .to_string();

    Ok(PushEvent {
        repo,
        branch,
        commit,
        clone_url,
    })
}
