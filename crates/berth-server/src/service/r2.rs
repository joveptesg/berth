//! Cloudflare R2 wizard (mvp-spec §6): one API token instead of hand-made S3 keys. The server
//! checks the token, creates the bucket, sets CORS for uploads from the admin and saves an S3
//! profile with presigned delivery. R2's S3 keys come from the token itself: the key id is the
//! token's id and the secret is the sha256 of the token value (Cloudflare docs, "S3 API
//! credentials from an API token").

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::storage::{self, ProfileInput};
use crate::AppState;

/// Cloudflare API.
pub const API: &str = "https://api.cloudflare.com/client/v4";
/// Where the admin creates a token with R2 permissions.
pub const TOKEN_PAGE: &str = "https://dash.cloudflare.com/profile/api-tokens";

/// What the admin enters.
#[derive(Debug, Clone, Default)]
pub struct R2Input {
    /// Profile name.
    pub name: String,
    /// Cloudflare account id (32 hex digits, on the R2 overview page).
    pub account_id: String,
    /// API token with "Workers R2 Storage: Edit".
    pub token: String,
    /// Bucket to use (created if missing).
    pub bucket: String,
}

/// S3 access keys of an R2 API token: `(access_key_id, secret_access_key)`.
#[must_use]
pub fn s3_keys(token_id: &str, token: &str) -> (String, String) {
    (
        token_id.to_owned(),
        hex::encode(Sha256::digest(token.as_bytes())),
    )
}

/// The S3 endpoint of an account.
#[must_use]
pub fn endpoint(account_id: &str) -> String {
    format!("https://{account_id}.r2.cloudflarestorage.com")
}

fn invalid(code: &str) -> BerthError {
    BerthError::Validation(code.into())
}

fn validate(input: &R2Input) -> Result<(String, String, String, String)> {
    let account = input.account_id.trim().to_ascii_lowercase();
    if account.len() != 32 || !account.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("r2_account_invalid"));
    }
    let token = input.token.trim().to_owned();
    if token.is_empty() {
        return Err(invalid("r2_token_required"));
    }
    // R2 bucket names: 3-63 characters, lowercase letters, digits and hyphens, not at the ends.
    let bucket = input.bucket.trim().to_owned();
    let valid = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !bucket.starts_with('-')
        && !bucket.ends_with('-');
    if !valid {
        return Err(invalid("r2_bucket_invalid"));
    }
    let name = match input.name.trim() {
        "" => "Cloudflare R2".to_owned(),
        name => name.to_owned(),
    };
    Ok((account, token, bucket, name))
}

/// A Cloudflare API reply: `Ok(result)` or the codes of its errors (empty for HTTP failures).
async fn call(request: reqwest::RequestBuilder) -> std::result::Result<Value, (u16, Vec<i64>)> {
    let response = request.send().await.map_err(|err| {
        tracing::info!(error = %err, "cloudflare API unreachable");
        (0, Vec::new())
    })?;
    let status = response.status().as_u16();
    let body: Value = response
        .bytes()
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if body["success"] == true {
        return Ok(body["result"].clone());
    }
    let codes = body["errors"]
        .as_array()
        .map(|e| e.iter().filter_map(|e| e["code"].as_i64()).collect())
        .unwrap_or_default();
    tracing::info!(status, ?codes, "cloudflare API refused");
    Err((status, codes))
}

/// The id of an active token: an account token first, then a user token.
async fn token_id(
    client: &reqwest::Client,
    api: &str,
    account: &str,
    token: &str,
) -> Result<String> {
    for url in [
        format!("{api}/accounts/{account}/tokens/verify"),
        format!("{api}/user/tokens/verify"),
    ] {
        if let Ok(result) = call(client.get(&url).bearer_auth(token)).await
            && result["status"] == "active"
            && let Some(id) = result["id"].as_str()
        {
            return Ok(id.to_owned());
        }
    }
    Err(invalid("r2_token_invalid"))
}

/// Connects R2: checks the token, creates the bucket (an existing one is fine), sets CORS and
/// saves the profile. `api` is the Cloudflare API base ([`API`]); `origins` may upload from a
/// browser. Returns the new profile id; the caller checks the connection.
///
/// # Errors
/// `Validation` with `r2_account_invalid`, `r2_token_required`, `r2_bucket_invalid`,
/// `r2_token_invalid`, `r2_permission_denied`, `r2_unreachable` or `r2_failed`, and the codes
/// of [`storage::create`].
pub async fn connect(
    state: &AppState,
    tenant_id: &str,
    input: &R2Input,
    api: &str,
    origins: Vec<String>,
) -> Result<String> {
    let (account, token, bucket, name) = validate(input)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("http client")?;
    let id = token_id(&client, api, &account, &token).await?;

    let failed = |(status, codes): (u16, Vec<i64>)| match status {
        0 => invalid("r2_unreachable"),
        401 | 403 => invalid("r2_permission_denied"),
        _ if codes.contains(&10000) => invalid("r2_permission_denied"),
        _ => invalid("r2_failed"),
    };
    let created = call(
        client
            .post(format!("{api}/accounts/{account}/r2/buckets"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(json!({ "name": bucket }).to_string()),
    )
    .await;
    match created {
        // 10004: the bucket exists already (ours, or made by the admin beforehand).
        Err((_, codes)) if codes.contains(&10004) => {}
        Err(err) => return Err(failed(err)),
        Ok(_) => {}
    }
    let origins = if origins.is_empty() {
        vec!["*".to_owned()]
    } else {
        origins
    };
    let cors = json!({
        "rules": [{
            "allowed": {
                "origins": origins,
                "methods": ["GET", "HEAD", "PUT"],
                "headers": ["*"],
            },
            "exposeHeaders": ["ETag", "Content-Length", "Content-Range"],
            "maxAgeSeconds": 3600,
        }]
    });
    call(
        client
            .put(format!("{api}/accounts/{account}/r2/buckets/{bucket}/cors"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(cors.to_string()),
    )
    .await
    .map_err(failed)?;

    let (access_key_id, secret_access_key) = s3_keys(&id, &token);
    storage::create(
        state,
        tenant_id,
        &ProfileInput {
            name,
            kind: "s3".into(),
            endpoint: endpoint(&account),
            bucket,
            region: "auto".into(),
            path_style: true,
            delivery_mode: "presigned".into(),
            access_key_id,
            secret_access_key,
            ..ProfileInput::default()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_endpoint_and_input() {
        let (id, secret) = s3_keys("abc", "token");
        assert_eq!(id, "abc");
        assert_eq!(
            secret,
            "3c469e9d6c5875d37a43f353d4f88e61fcf812c66eee3457465a40b0da4153e0"
        );
        assert_eq!(
            endpoint("0123456789abcdef0123456789abcdef"),
            "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com"
        );
        let good = R2Input {
            account_id: "0123456789ABCDEF0123456789abcdef".into(),
            token: " t ".into(),
            bucket: "berth-books".into(),
            ..R2Input::default()
        };
        let (account, token, _, name) = validate(&good).unwrap();
        assert_eq!(
            (account.as_str(), token.as_str()),
            ("0123456789abcdef0123456789abcdef", "t")
        );
        assert_eq!(name, "Cloudflare R2");
        for (bucket, code) in [
            ("Bad_Name", "r2_bucket_invalid"),
            ("-x-", "r2_bucket_invalid"),
        ] {
            let input = R2Input {
                bucket: bucket.into(),
                ..good.clone()
            };
            assert!(matches!(validate(&input), Err(BerthError::Validation(c)) if c == code));
        }
        let short = R2Input {
            account_id: "123".into(),
            ..good
        };
        assert!(
            matches!(validate(&short), Err(BerthError::Validation(c)) if c == "r2_account_invalid")
        );
    }
}
