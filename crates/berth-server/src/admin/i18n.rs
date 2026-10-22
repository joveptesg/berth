//! Admin localization: `rust-i18n` catalogs in `locales/`, English fallback.
//!
//! [`locale_layer`] resolves the locale (cookie `berth_locale`, then `Accept-Language`) and binds
//! it to a task-local for the request; the `MiniJinja` `t()` function reads it back. A task-local
//! follows the request across `.await` and never leaks between concurrent requests, unlike
//! `rust_i18n::set_locale`, which is process-global.

use axum::{extract::Request, http::header, middleware::Next, response::Response};

const COOKIE: &str = "berth_locale";
const DEFAULT: &str = "en";

tokio::task_local! {
    static CURRENT_LOCALE: String;
}

/// Locale of the current request; `en` outside a request (tests, startup).
pub(crate) fn current_locale() -> String {
    CURRENT_LOCALE
        .try_with(Clone::clone)
        .unwrap_or_else(|_| DEFAULT.to_owned())
}

fn resolve(req: &Request) -> String {
    let available = rust_i18n::available_locales!();
    let supported: Vec<&str> = available.iter().map(AsRef::as_ref).collect();
    if let Some(choice) = super::cookie(req.headers(), COOKIE)
        && supported.contains(&choice)
    {
        return choice.to_owned();
    }
    let accept = req
        .headers()
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    accept_language::intersection(accept, &supported)
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT.to_owned())
}

/// Middleware binding the request locale.
pub(crate) async fn locale_layer(req: Request, next: Next) -> Response {
    let locale = resolve(&req);
    CURRENT_LOCALE.scope(locale, next.run(req)).await
}

/// Runs `f` with the locale bound, as [`locale_layer`] does for requests.
#[cfg(test)]
pub(crate) async fn with_locale<F: Future>(locale: &str, f: F) -> F::Output {
    CURRENT_LOCALE.scope(locale.to_owned(), f).await
}

/// `MiniJinja` `t("key", name=value)`: catalog lookup with `%{name}` placeholders.
#[expect(
    clippy::needless_pass_by_value,
    reason = "MiniJinja passes keyword arguments by value"
)]
pub(crate) fn translate(
    key: &str,
    kwargs: minijinja::value::Kwargs,
) -> Result<String, minijinja::Error> {
    let mut text = rust_i18n::t!(key, locale = &current_locale()).into_owned();
    for name in kwargs.args() {
        let value: minijinja::Value = kwargs.get(name)?;
        text = text.replace(&format!("%{{{name}}}"), &value.to_string());
    }
    kwargs.assert_all_used()?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::*;

    fn request(cookie: Option<&str>, accept: &str) -> Request {
        let mut builder = Request::builder().header(header::ACCEPT_LANGUAGE, accept);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn cookie_beats_accept_language() {
        assert_eq!(resolve(&request(None, "ru-RU,ru;q=0.9,en;q=0.8")), "ru");
        assert_eq!(resolve(&request(Some("a=1; berth_locale=en"), "ru")), "en");
        assert_eq!(resolve(&request(Some("berth_locale=de"), "ru")), "ru");
        assert_eq!(resolve(&request(None, "de")), "en");
    }

    /// Dotted key paths of a YAML catalog (nested maps with two-space indent).
    fn keys(file: &str) -> Vec<String> {
        let path = format!("{}/locales/{file}", env!("CARGO_MANIFEST_DIR"));
        let mut stack: Vec<String> = Vec::new();
        let mut keys = Vec::new();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let trimmed = line.trim_start();
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            if trimmed.starts_with('#') || key.contains(' ') {
                continue;
            }
            stack.truncate((line.len() - trimmed.len()) / 2);
            stack.push(key.to_owned());
            if !value.trim().is_empty() {
                keys.push(stack.join("."));
            }
        }
        keys.sort();
        keys
    }

    #[test]
    fn catalogs_have_the_same_keys() {
        assert_eq!(keys("en.yml"), keys("ru.yml"));
        assert_eq!(rust_i18n::t!("nav.settings", locale = "en"), "Settings");
        assert_eq!(rust_i18n::t!("nav.settings", locale = "ru"), "Настройки");
    }
}
