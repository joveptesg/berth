//! Presigned S3 URLs (query-string `SigV4`) for direct delivery and browser uploads.
//!
//! `object_store` cannot sign extra headers, so URLs are signed here with `aws-sigv4`.
//! A presigned PUT signs `content-length` and `x-amz-checksum-sha256`: the client must send
//! exactly these values, and S3 rejects a body whose sha256 differs (design doc §22.A5).

use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{
        PercentEncodingMode, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
        UriPathNormalizationMode, sign,
    },
    sign::v4,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use url::Url;

/// Signs URLs of one bucket.
#[derive(Clone)]
pub(crate) struct Presigner {
    /// `https://host/bucket` (path style) or `https://bucket.host` (virtual-hosted style).
    pub(crate) base: Url,
    pub(crate) region: String,
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
}

/// Headers the client must send with a presigned PUT, exactly as signed.
#[must_use]
pub fn put_headers(sha256: &[u8; 32], size: u64) -> [(&'static str, String); 2] {
    [
        ("content-length", size.to_string()),
        ("x-amz-checksum-sha256", STANDARD.encode(sha256)),
    ]
}

impl Presigner {
    /// URL of an object key (`/`-separated, already safe for a URL path).
    pub(crate) fn object_url(&self, key: &str) -> Url {
        let mut url = self.base.clone();
        let path = format!("{}/{key}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url
    }

    pub(crate) fn get(&self, key: &str, ttl: Duration, now: SystemTime) -> anyhow::Result<Url> {
        self.sign("GET", self.object_url(key), &[], ttl, now)
    }

    pub(crate) fn put(
        &self,
        key: &str,
        sha256: &[u8; 32],
        size: u64,
        ttl: Duration,
        now: SystemTime,
    ) -> anyhow::Result<Url> {
        let headers = put_headers(sha256, size);
        let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.sign("PUT", self.object_url(key), &headers, ttl, now)
    }

    fn sign(
        &self,
        method: &str,
        mut url: Url,
        headers: &[(&str, &str)],
        ttl: Duration,
        now: SystemTime,
    ) -> anyhow::Result<Url> {
        let identity = Credentials::new(
            &self.access_key_id,
            &self.secret_access_key,
            None,
            None,
            "berth",
        )
        .into();
        let mut settings = SigningSettings::default();
        settings.signature_location = SignatureLocation::QueryParams;
        settings.expires_in = Some(ttl);
        // S3 signs the path as sent, without normalization or double encoding.
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(now)
            .settings(settings)
            .build()?
            .into();
        let request = SignableRequest::new(
            method,
            url.as_str(),
            headers.iter().copied(),
            SignableBody::UnsignedPayload,
        )?;
        let (instructions, _signature) = sign(request, &params)?.into_parts();
        let (_, query) = instructions.into_parts();
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_ref())));
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(base: &str) -> Presigner {
        Presigner {
            base: Url::parse(base).unwrap(),
            region: "us-east-1".into(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    fn query(url: &Url, key: &str) -> String {
        url.query_pairs()
            .find_map(|(k, v)| (k == key).then(|| v.into_owned()))
            .unwrap_or_else(|| panic!("{key} not in {url}"))
    }

    /// The presigned GET example from the AWS S3 `SigV4` documentation.
    #[test]
    fn matches_the_aws_example() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(380_376); // 2013-05-24
        let url = example("https://examplebucket.s3.amazonaws.com")
            .get("test.txt", Duration::from_hours(24), now)
            .unwrap();
        assert_eq!(url.path(), "/test.txt");
        assert_eq!(query(&url, "X-Amz-SignedHeaders"), "host");
        assert_eq!(
            query(&url, "X-Amz-Signature"),
            "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn put_signs_size_and_checksum() {
        let url = example("http://127.0.0.1:9000/books")
            .put(
                "t1/blobs/sha256/ab/cd/abcd",
                &[0xab; 32],
                1234,
                Duration::from_mins(15),
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(url.path(), "/books/t1/blobs/sha256/ab/cd/abcd");
        assert_eq!(
            query(&url, "X-Amz-SignedHeaders"),
            "content-length;host;x-amz-checksum-sha256"
        );
        assert_eq!(query(&url, "X-Amz-Expires"), "900");
        assert_eq!(
            put_headers(&[0xab; 32], 1234)[1].1,
            "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s="
        );
    }
}
