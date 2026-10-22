//! Device tokens (design doc §17): short-lived JWT access tokens and the refresh token chain.
//!
//! A refresh token is `<device_id>.<random>`; only its sha256 is stored. Rotation does not store
//! the successor's secret: it is HMAC-SHA256(`secret.key`, `device_id || token_hash`), so a second
//! refresh with the same token inside the grace period gets the very same successor. The device
//! id prefix lets a refresh of a deleted chain tell `device_revoked` from `session_expired`.

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use berth_core::{AuthError, BerthError, Result};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use subtle::ConstantTimeEq;

use super::{random_token, token_hash};
use crate::{SecretKey, secret::hmac_sha256};

/// Lifetime of an access token in seconds (design doc §17: at most 30 minutes).
pub const ACCESS_TTL_S: i64 = 15 * 60;
/// Sliding lifetime of a refresh token: every rotation extends the session by this much.
const REFRESH_TTL_MS: i64 = 60 * 24 * 60 * 60 * 1000;
/// A rotated token presented again within this window gets the same successor (parallel
/// refreshes of the player and the sync); later reuse is treated as theft.
pub const GRACE_MS: i64 = 45_000;

/// `base64url({"alg":"HS256","typ":"JWT"})`; the only header accepted.
const JWT_HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
/// Access tokens are signed with a key derived from `secret.key`, separate from other HMAC uses.
const JWT_KEY_LABEL: &[u8] = b"berth access token v1";

/// Claims of an access token: identification only, access is checked live on every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Tenant id.
    pub tenant: String,
    /// User id.
    pub user: String,
    /// Device id.
    pub device: String,
    /// Issued at, epoch seconds.
    pub iat: i64,
    /// Expires at, epoch seconds.
    pub exp: i64,
}

/// Tokens handed to a device after pairing or refresh.
#[derive(Debug, Clone)]
pub struct TokenPair {
    /// JWT access token.
    pub access_token: String,
    /// Opaque refresh token.
    pub refresh_token: String,
    /// Seconds until the access token expires; the client refreshes ahead of it.
    pub access_expires_in: i64,
}

/// A device authenticated by its access token.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    /// Tenant id.
    pub tenant_id: String,
    /// User id.
    pub user_id: String,
    /// Device id.
    pub device_id: String,
    /// Scopes of the device (`listen`, `upload`, `admin`, `feed`).
    pub scopes: Vec<String>,
}

impl DeviceAuth {
    /// Checks that the device has `scope`.
    ///
    /// # Errors
    /// `Forbidden` without the scope.
    pub fn require(&self, scope: &str) -> Result<()> {
        if self.scopes.iter().any(|s| s == scope) {
            Ok(())
        } else {
            Err(BerthError::Forbidden)
        }
    }
}

fn jwt_signature(secret: &SecretKey, signed: &str) -> [u8; 32] {
    let key = secret.hmac(&[JWT_KEY_LABEL]);
    hmac_sha256(&key, &[signed.as_bytes()])
}

/// Signs an HS256 JWT.
pub(crate) fn sign_access(secret: &SecretKey, claims: &Claims) -> anyhow::Result<String> {
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let signed = format!("{JWT_HEADER}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(jwt_signature(secret, &signed));
    Ok(format!("{signed}.{signature}"))
}

/// Verifies signature and expiry of an access token.
pub(crate) fn verify_access(
    secret: &SecretKey,
    token: &str,
    now_s: i64,
) -> Result<Claims, AuthError> {
    let invalid = AuthError::Unauthenticated;
    let (signed, signature) = token.rsplit_once('.').ok_or(invalid)?;
    let (header, payload) = signed.split_once('.').ok_or(invalid)?;
    // A fixed header rules out algorithm confusion (`none`, RS256 with the HMAC key).
    if header != JWT_HEADER {
        return Err(invalid);
    }
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid)?;
    if !bool::from(jwt_signature(secret, signed).ct_eq(signature.as_slice())) {
        return Err(invalid);
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid)?;
    let claims: Claims = serde_json::from_slice(&payload).map_err(|_| invalid)?;
    if claims.exp <= now_s {
        return Err(invalid);
    }
    Ok(claims)
}

/// The deterministic successor of the token with `hash`.
fn successor(secret: &SecretKey, device_id: &str, hash: &str) -> String {
    let mac = secret.hmac(&[device_id.as_bytes(), hash.as_bytes()]);
    format!("{device_id}.{}", URL_SAFE_NO_PAD.encode(mac))
}

fn access_pair(
    secret: &SecretKey,
    tenant: &str,
    user: &str,
    device: &str,
    refresh_token: String,
    now_ms: i64,
) -> anyhow::Result<TokenPair> {
    let issued = now_ms / 1000;
    let claims = Claims {
        tenant: tenant.to_owned(),
        user: user.to_owned(),
        device: device.to_owned(),
        iat: issued,
        exp: issued + ACCESS_TTL_S,
    };
    Ok(TokenPair {
        access_token: sign_access(secret, &claims)?,
        refresh_token,
        access_expires_in: ACCESS_TTL_S,
    })
}

async fn insert_refresh(
    conn: &mut SqliteConnection,
    token: &str,
    tenant_id: &str,
    device_id: &str,
    now_ms: i64,
) -> sqlx::Result<()> {
    // The grace path may race the winner to insert the same successor.
    sqlx::query(
        "INSERT INTO refresh_tokens (token_hash, tenant_id, device_id, issued_at, expires_at)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (token_hash) DO NOTHING",
    )
    .bind(token_hash(token))
    .bind(tenant_id)
    .bind(device_id)
    .bind(now_ms)
    .bind(now_ms + REFRESH_TTL_MS)
    .execute(conn)
    .await?;
    Ok(())
}

/// Starts the token chain of a newly paired device.
pub(crate) async fn issue(
    conn: &mut SqliteConnection,
    secret: &SecretKey,
    tenant_id: &str,
    user_id: &str,
    device_id: &str,
    now_ms: i64,
) -> anyhow::Result<TokenPair> {
    let refresh = format!("{device_id}.{}", random_token()?);
    insert_refresh(conn, &refresh, tenant_id, device_id, now_ms).await?;
    access_pair(secret, tenant_id, user_id, device_id, refresh, now_ms)
}

/// Owner and revocation state of a device.
async fn device_state(db: &SqlitePool, device_id: &str) -> sqlx::Result<Option<(String, bool)>> {
    let row: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT user_id, revoked_at FROM devices WHERE id = $1")
            .bind(device_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(user, revoked)| (user, revoked.is_some())))
}

/// Exchanges a refresh token for a new pair (rotation with grace and theft detection).
///
/// # Errors
/// `Auth(DeviceRevoked)` for a revoked device; `Auth(SessionExpired)` for an expired, unknown
/// or stolen token (reuse after the grace period deletes the device's chain).
pub async fn refresh(
    db: &SqlitePool,
    secret: &SecretKey,
    token: &str,
    now_ms: i64,
) -> Result<TokenPair> {
    let hash = token_hash(token);

    // Write first: the conditional UPDATE decides the winner of concurrent refreshes and
    // takes the write lock before anything is read.
    let mut tx = db.begin().await.context("begin")?;
    let won: Option<(String, String)> = sqlx::query_as(
        "UPDATE refresh_tokens SET rotated_at = $2
         WHERE token_hash = $1 AND rotated_at IS NULL AND expires_at > $2
         RETURNING tenant_id, device_id",
    )
    .bind(&hash)
    .bind(now_ms)
    .fetch_optional(&mut *tx)
    .await
    .context("rotate refresh token")?;
    if let Some((tenant_id, device_id)) = won {
        let (user_id, revoked): (String, Option<i64>) =
            sqlx::query_as("SELECT user_id, revoked_at FROM devices WHERE id = $1")
                .bind(&device_id)
                .fetch_one(&mut *tx)
                .await
                .context("load device")?;
        if revoked.is_some() {
            return Err(AuthError::DeviceRevoked.into());
        }
        let next = successor(secret, &device_id, &hash);
        insert_refresh(&mut tx, &next, &tenant_id, &device_id, now_ms)
            .await
            .context("insert successor")?;
        sqlx::query("UPDATE devices SET last_seen_at = $2 WHERE id = $1")
            .bind(&device_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .context("touch device")?;
        tx.commit().await.context("commit refresh")?;
        return Ok(access_pair(
            secret, &tenant_id, &user_id, &device_id, next, now_ms,
        )?);
    }
    tx.rollback().await.context("rollback")?;

    let row: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT tenant_id, device_id, expires_at, rotated_at FROM refresh_tokens
         WHERE token_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await
    .context("load refresh token")?;
    let Some((tenant_id, device_id, expires_at, rotated_at)) = row else {
        // Unknown: expired and purged, or its chain was deleted. Revocation also deletes the
        // chain, so the device id prefix decides which error the client gets.
        let device = token.split_once('.').map_or("", |(device, _)| device);
        return Err(
            match device_state(db, device).await.context("load device")? {
                Some((_, true)) => AuthError::DeviceRevoked,
                _ => AuthError::SessionExpired,
            }
            .into(),
        );
    };
    let (user_id, revoked) = device_state(db, &device_id)
        .await
        .context("load device")?
        .context("refresh token of a missing device")?;
    if revoked {
        return Err(AuthError::DeviceRevoked.into());
    }
    match rotated_at {
        _ if expires_at <= now_ms => Err(AuthError::SessionExpired.into()),
        Some(rotated) if now_ms - rotated <= GRACE_MS => {
            let next = successor(secret, &device_id, &hash);
            Ok(access_pair(
                secret, &tenant_id, &user_id, &device_id, next, now_ms,
            )?)
        }
        Some(_) => {
            sqlx::query("DELETE FROM refresh_tokens WHERE device_id = $1")
                .bind(&device_id)
                .execute(db)
                .await
                .context("delete stolen chain")?;
            tracing::warn!(
                device_id,
                "refresh token reused after rotation: possible theft, device must pair again"
            );
            Err(AuthError::SessionExpired.into())
        }
        // Rotated by nobody yet failed the UPDATE above: a concurrent winner rolled back.
        None => Err(AuthError::SessionExpired.into()),
    }
}

/// Resolves an access token to a live device.
///
/// # Errors
/// `Auth(Unauthenticated)` for a bad or expired token or a missing device,
/// `Auth(DeviceRevoked)` for a revoked device.
pub async fn authenticate(
    db: &SqlitePool,
    secret: &SecretKey,
    token: &str,
    now_ms: i64,
) -> Result<DeviceAuth> {
    let claims = verify_access(secret, token, now_ms / 1000)?;
    let row: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT user_id, revoked_at FROM devices WHERE id = $1 AND tenant_id = $2")
            .bind(&claims.device)
            .bind(&claims.tenant)
            .fetch_optional(db)
            .await
            .context("load device")?;
    let Some((user_id, revoked)) = row else {
        return Err(AuthError::Unauthenticated.into());
    };
    if revoked.is_some() {
        return Err(AuthError::DeviceRevoked.into());
    }
    let scopes = sqlx::query_scalar("SELECT scope FROM device_scopes WHERE device_id = $1")
        .bind(&claims.device)
        .fetch_all(db)
        .await
        .context("load scopes")?;
    Ok(DeviceAuth {
        tenant_id: claims.tenant,
        user_id,
        device_id: claims.device,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    const NOW: i64 = 1_800_000_000_000;

    fn secret() -> SecretKey {
        SecretKey::from_bytes([7; 32])
    }

    async fn paired() -> (tempfile::TempDir, SqlitePool, TokenPair) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::connect(&dir.path().join("t.sqlite")).await.unwrap();
        for sql in [
            "INSERT INTO tenants VALUES ('t', 'T', 0)",
            "INSERT INTO users (id, tenant_id, name, role, created_at) VALUES ('u', 't', 'U', 'member', 0)",
            "INSERT INTO devices (id, tenant_id, user_id, name, platform, paired_at)
             VALUES ('d', 't', 'u', 'Phone', 'android', 0)",
            "INSERT INTO device_scopes VALUES ('t', 'd', 'listen')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let mut conn = pool.acquire().await.unwrap();
        let pair = issue(&mut conn, &secret(), "t", "u", "d", NOW)
            .await
            .unwrap();
        drop(conn);
        (dir, pool, pair)
    }

    async fn chain_len(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM refresh_tokens WHERE device_id = 'd'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn auth_error(result: Result<TokenPair>) -> AuthError {
        match result {
            Err(BerthError::Auth(e)) => e,
            other => panic!("expected an auth error, got {other:?}"),
        }
    }

    #[test]
    fn access_token_round_trip() {
        let claims = Claims {
            tenant: "t".into(),
            user: "u".into(),
            device: "d".into(),
            iat: 100,
            exp: 200,
        };
        let token = sign_access(&secret(), &claims).unwrap();
        assert_eq!(verify_access(&secret(), &token, 199).unwrap(), claims);
        assert!(verify_access(&secret(), &token, 200).is_err(), "expired");
        assert!(
            verify_access(&SecretKey::from_bytes([8; 32]), &token, 150).is_err(),
            "other key"
        );
        let (signed, _) = token.rsplit_once('.').unwrap();
        let none = format!("eyJhbGciOiJub25lIn0.{}.", signed.split_once('.').unwrap().1);
        assert!(verify_access(&secret(), &none, 150).is_err(), "alg none");
        let mut tampered = token.clone();
        tampered.insert(JWT_HEADER.len() + 3, 'x');
        assert!(verify_access(&secret(), &tampered, 150).is_err(), "payload");
    }

    #[tokio::test]
    async fn rotation_issues_a_new_token_and_extends_the_session() {
        let (_dir, pool, first) = paired().await;
        let second = refresh(&pool, &secret(), &first.refresh_token, NOW + 1000)
            .await
            .unwrap();
        assert_ne!(second.refresh_token, first.refresh_token);
        assert!(second.refresh_token.starts_with("d."));
        assert_eq!(second.access_expires_in, ACCESS_TTL_S);
        let claims = verify_access(&secret(), &second.access_token, NOW / 1000).unwrap();
        assert_eq!((claims.tenant.as_str(), claims.device.as_str()), ("t", "d"));
        // Long after the first token's 60 days, the rotated one still works.
        let later = NOW + REFRESH_TTL_MS - 1;
        let third = refresh(&pool, &secret(), &second.refresh_token, later)
            .await
            .unwrap();
        refresh(
            &pool,
            &secret(),
            &third.refresh_token,
            later + REFRESH_TTL_MS - 1,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_refresh_in_grace_gets_the_same_token() {
        let (_dir, pool, first) = paired().await;
        let key = secret();
        let (a, b) = tokio::join!(
            refresh(&pool, &key, &first.refresh_token, NOW + 1000),
            refresh(&pool, &key, &first.refresh_token, NOW + 1000),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.refresh_token, b.refresh_token);
        let late = refresh(
            &pool,
            &secret(),
            &first.refresh_token,
            NOW + 1000 + GRACE_MS,
        )
        .await
        .unwrap();
        assert_eq!(late.refresh_token, a.refresh_token, "still within grace");
        // No revocation: the successor keeps working.
        assert!(
            refresh(&pool, &secret(), &a.refresh_token, NOW + 2000)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn reuse_after_grace_deletes_the_chain() {
        let (_dir, pool, first) = paired().await;
        let second = refresh(&pool, &secret(), &first.refresh_token, NOW)
            .await
            .unwrap();
        let theft = refresh(&pool, &secret(), &first.refresh_token, NOW + GRACE_MS + 1).await;
        assert_eq!(auth_error(theft), AuthError::SessionExpired);
        assert_eq!(chain_len(&pool).await, 0);
        // The legitimate holder has to pair again as well.
        let after = refresh(&pool, &secret(), &second.refresh_token, NOW + GRACE_MS + 2).await;
        assert_eq!(auth_error(after), AuthError::SessionExpired);
    }

    #[tokio::test]
    async fn expired_and_unknown_tokens_mean_session_expired() {
        let (_dir, pool, first) = paired().await;
        let expired = refresh(&pool, &secret(), &first.refresh_token, NOW + REFRESH_TTL_MS).await;
        assert_eq!(auth_error(expired), AuthError::SessionExpired);
        let unknown = refresh(&pool, &secret(), "d.made-up", NOW).await;
        assert_eq!(auth_error(unknown), AuthError::SessionExpired);
        assert_eq!(
            auth_error(refresh(&pool, &secret(), "garbage", NOW).await),
            AuthError::SessionExpired
        );
    }

    #[tokio::test]
    async fn revoked_device_gets_device_revoked() {
        let (_dir, pool, first) = paired().await;
        let auth = authenticate(&pool, &secret(), &first.access_token, NOW)
            .await
            .unwrap();
        assert_eq!(auth.scopes, ["listen"]);
        assert!(auth.require("listen").is_ok());
        assert!(matches!(auth.require("admin"), Err(BerthError::Forbidden)));

        // Revocation deletes the chain; the token prefix still names the device.
        sqlx::query("UPDATE devices SET revoked_at = 1 WHERE id = 'd'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM refresh_tokens WHERE device_id = 'd'")
            .execute(&pool)
            .await
            .unwrap();
        let refreshed = refresh(&pool, &secret(), &first.refresh_token, NOW).await;
        assert_eq!(auth_error(refreshed), AuthError::DeviceRevoked);
        let access = authenticate(&pool, &secret(), &first.access_token, NOW).await;
        assert!(matches!(
            access,
            Err(BerthError::Auth(AuthError::DeviceRevoked))
        ));
    }
}
