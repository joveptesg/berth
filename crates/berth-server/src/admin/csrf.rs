//! CSRF protection for state-changing admin requests: the browser must say the request is
//! same-origin (`Sec-Fetch-Site`), or, for browsers without it, send an `Origin` matching `Host`.

use axum::{
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

pub(crate) async fn check(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        || is_same_origin(req.headers())
    {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response()
    }
}

fn is_same_origin(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(site) = header("sec-fetch-site") {
        return matches!(site, "same-origin" | "none");
    }
    let (Some(origin), Some(host)) = (
        header(header::ORIGIN.as_str()),
        header(header::HOST.as_str()),
    ) else {
        return false;
    };
    url::Url::parse(origin).is_ok_and(|origin| {
        let authority = match origin.port() {
            Some(port) => format!("{}:{port}", origin.host_str().unwrap_or_default()),
            None => origin.host_str().unwrap_or_default().to_owned(),
        };
        authority.eq_ignore_ascii_case(host)
    })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), HeaderValue::from_static(v)))
            .collect()
    }

    #[test]
    fn accepts_only_same_origin() {
        assert!(is_same_origin(&headers(&[(
            "sec-fetch-site",
            "same-origin"
        )])));
        assert!(!is_same_origin(&headers(&[(
            "sec-fetch-site",
            "cross-site"
        )])));
        assert!(!is_same_origin(&headers(&[
            ("sec-fetch-site", "same-site"),
            ("origin", "http://berth.local:8080"),
            ("host", "berth.local:8080"),
        ])));
        assert!(is_same_origin(&headers(&[
            ("origin", "http://192.168.0.4:8080"),
            ("host", "192.168.0.4:8080"),
        ])));
        assert!(is_same_origin(&headers(&[
            ("origin", "https://books.example"),
            ("host", "books.example"),
        ])));
        assert!(!is_same_origin(&headers(&[
            ("origin", "https://evil.example"),
            ("host", "books.example"),
        ])));
        assert!(!is_same_origin(&headers(&[("host", "books.example")])));
    }
}
