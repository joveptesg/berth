//! Admin localization: `rust-i18n` catalogs in `locales/`, English fallback.
//!
//! [`locale_layer`] resolves the locale (cookie `berth_locale`, then `Accept-Language`) and binds
//! it to a task-local for the request; the `MiniJinja` `t()` function reads it back. A task-local
//! follows the request across `.await` and never leaks between concurrent requests, unlike
//! `rust_i18n::set_locale`, which is process-global.

use axum::{extract::Request, http::header, middleware::Next, response::Response};

const COOKIE: &str = "berth_locale";
const DEFAULT: &str = "en";

/// Admin languages in switcher order, each named in itself. A catalog `locales/<code>.yml` each.
pub(crate) const LOCALES: [(&str, &str); 10] = [
    ("en", "English"),
    ("ru", "Русский"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("pt-BR", "Português (Brasil)"),
    ("zh-Hans", "简体中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
];

/// The catalog for a browser language tag: `zh-CN` → `zh-Hans`, `pt-PT` → `pt-BR`, `de-AT` → `de`.
fn catalog_for(tag: &str) -> Option<&'static str> {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let code = match primary.as_str() {
        "zh" => "zh-Hans",
        "pt" => "pt-BR",
        other => other,
    };
    LOCALES.iter().map(|(c, _)| *c).find(|c| *c == code)
}

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
    if let Some(choice) = super::cookie(req.headers(), COOKIE)
        && LOCALES.iter().any(|(code, _)| *code == choice)
    {
        return choice.to_owned();
    }
    let accept = req
        .headers()
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    accept_language::parse(accept)
        .iter()
        .find_map(|tag| catalog_for(tag))
        .unwrap_or(DEFAULT)
        .to_owned()
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
        assert_eq!(resolve(&request(Some("berth_locale=xx"), "ru")), "ru");
        assert_eq!(resolve(&request(None, "nl")), "en");
    }

    #[test]
    fn browser_tags_map_to_catalogs() {
        assert_eq!(resolve(&request(None, "zh-CN,zh;q=0.9")), "zh-Hans");
        assert_eq!(resolve(&request(None, "pt-PT")), "pt-BR");
        assert_eq!(resolve(&request(None, "nl,de-AT;q=0.8")), "de");
        assert_eq!(resolve(&request(Some("berth_locale=ja"), "en")), "ja");
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
        let available = rust_i18n::available_locales!();
        for (code, _) in LOCALES {
            assert_eq!(keys("en.yml"), keys(&format!("{code}.yml")), "{code}");
            assert!(available.iter().any(|l| *l == code), "{code} not loaded");
        }
        assert_eq!(rust_i18n::t!("nav.settings", locale = "en"), "Settings");
        assert_eq!(rust_i18n::t!("nav.settings", locale = "ru"), "Настройки");
    }
}
