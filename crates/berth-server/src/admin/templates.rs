//! `MiniJinja` environment. Debug builds read templates from disk and reload them on change;
//! release builds embed them.

use minijinja::Environment;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/templates/"]
struct TemplateAssets;

pub(crate) fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    if cfg!(debug_assertions) {
        env.set_loader(minijinja::path_loader(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/templates"
        )));
    } else {
        env.set_loader(|name| {
            Ok::<_, minijinja::Error>(
                TemplateAssets::get(name)
                    .map(|file| String::from_utf8_lossy(&file.data).into_owned()),
            )
        });
        env.set_auto_reload(false);
    }
    env.add_global("version", env!("CARGO_PKG_VERSION"));
    env.add_global("assets", super::static_files::fingerprint());
    env.add_function("t", super::i18n::translate);
    env.add_function("current_locale", super::i18n::current_locale);
    env.add_global(
        "locales",
        super::i18n::LOCALES
            .iter()
            .map(|&(code, name)| minijinja::Value::from(vec![code, name]))
            .collect::<minijinja::Value>(),
    );
    env.add_filter("clock", clock);
    env.add_filter("filesize", filesize);
    env.add_filter("num", num);
    env
}

/// A number without a pointless `.0`: series numbers 2 and 2.5 (2,5 where the locale says so).
fn num(value: f64) -> String {
    let text = value.to_string();
    localize_decimal(text.strip_suffix(".0").unwrap_or(&text))
}

/// Swaps the decimal dot for the locale's separator (`format.decimal` in the catalog).
fn localize_decimal(text: &str) -> String {
    let locale = super::i18n::current_locale();
    text.replace('.', &rust_i18n::t!("format.decimal", locale = &locale))
}

/// Milliseconds as `H:MM:SS` (or `M:SS` under an hour).
fn clock(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Bytes in KB, MB or GB with one decimal; units and separator from the locale.
fn filesize(bytes: i64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "a rounded display value")]
    let value = bytes.max(0) as f64;
    let (number, unit) = match bytes.max(0) {
        0..1_000_000 => (format!("{:.0}", value / 1e3), "format.kb"),
        1_000_000..1_000_000_000 => (format!("{:.1}", value / 1e6), "format.mb"),
        _ => (format!("{:.2}", value / 1e9), "format.gb"),
    };
    let unit = rust_i18n::t!(unit, locale = &super::i18n::current_locale());
    format!("{}\u{a0}{unit}", localize_decimal(&number))
}

#[cfg(test)]
mod tests {
    use minijinja::context;

    use super::*;

    #[test]
    fn formats_times_and_sizes() {
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(3_725_000), "1:02:05");
        assert_eq!(filesize(36_092), "36\u{a0}KB");
        assert_eq!(filesize(21_200_000), "21.2\u{a0}MB");
        assert_eq!(filesize(5_200_000_000), "5.20\u{a0}GB");
        assert_eq!(num(2.0), "2");
    }

    #[tokio::test]
    async fn sizes_follow_the_locale() {
        super::super::i18n::with_locale("ru", async {
            assert_eq!(filesize(21_200_000), "21,2\u{a0}МБ");
            assert_eq!(num(2.5), "2,5");
        })
        .await;
    }

    #[test]
    fn all_templates_parse() {
        let env = environment();
        let names: Vec<_> = TemplateAssets::iter().collect();
        assert!(names.len() >= 8, "templates found: {names:?}");
        for name in names {
            env.get_template(&name)
                .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }

    fn render(locale: &str, name: &str, ctx: minijinja::Value) -> String {
        let env = environment();
        let template = env.get_template(name).unwrap();
        let rendered = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(super::super::i18n::with_locale(locale, async {
                template.render(ctx)
            }));
        rendered.unwrap_or_else(|e| panic!("{name} ({locale}): {e:#}"))
    }

    #[test]
    fn user_settings_dialog_renders() {
        for locale in ["en", "ru"] {
            let dialog = render(
                locale,
                "partials/user_settings.html",
                context! {
                    user_settings => context! {
                        user => context! { id => "u2", name => "Anna" },
                        books => vec![
                            context! { id => "b1", title => "Book's \"one\"", authors => vec!["A"], cover_hash => () },
                        ],
                        picked => vec!["b1"],
                        access => "selected",
                        login_on => true,
                        login => "anna",
                        has_password => true,
                        min_password => 4,
                        error => "login_taken",
                        languages => vec![
                            context! { code => "ru", name => "Русский" },
                            context! { code => "en", name => "English" },
                        ],
                        spoken => vec!["en"],
                        sections => vec![context! { id => "c1", name => "Демо", closed => true }],
                        given => vec!["c1"],
                    },
                },
            );
            assert!(dialog.contains("/users/u2/settings") && dialog.contains("role=\"alert\""));
            // Alpine expressions sit in single-quoted attributes: a title's quote is escaped.
            assert!(dialog.contains("book\\u0027s"), "{dialog}");
            assert!(dialog.contains("picked: [\"b1\"]"), "{dialog}");
            assert!(
                dialog.contains("spoken: [\"en\"]") && dialog.contains(">English"),
                "{dialog}"
            );
            assert!(
                dialog.contains("given: [\"c1\"]") && dialog.contains("Демо"),
                "{dialog}"
            );
        }
    }

    #[test]
    fn pages_render_in_both_locales() {
        let base = context! {
            server_name => "Home", page => "dashboard", library_bytes => 0, user_count => 1,
            settings => context! { name => "Home", lan_url => "", wan_url => "" },
            locked => context! { name => false, lan_url => false, wan_url => true },
            code => "K7P29QXM",
        };
        for (locale, dashboard, login) in [("en", "Dashboard", "Sign in"), ("ru", "Обзор", "Войти")]
        {
            let html = render(locale, "dashboard.html", base.clone());
            assert!(html.contains(dashboard), "{locale}: {html}");
            assert!(html.contains(&format!("lang=\"{locale}\"")));
            assert!(render(locale, "login.html", base.clone()).contains(login));
            assert!(render(locale, "setup.html", base.clone()).contains("K7P29QXM"));
            let settings = render(locale, "settings.html", base.clone());
            assert!(settings.contains("name=\"wan_url\""));
            assert!(settings.contains("readonly"));
            assert!(!settings.contains("name=\"current\""), "one tab at a time");
            let password = render(
                locale,
                "settings.html",
                context! { tab => "password", ..base.clone() },
            );
            assert!(password.contains("name=\"current\""));
            assert!(!password.contains("name=\"wan_url\""));

            let users = context! {
                users => vec![
                    context! { id => "u1", name => "Admin", role => "owner", devices => 0, login => (), book_access => "all", books => 0 },
                    context! { id => "u2", name => "Anna", role => "member", devices => 2, login => "anna", book_access => "selected", books => 1 },
                ],
                ..base.clone()
            };
            let list = render(locale, "users.html", users.clone());
            assert!(list.contains("/users/u2/connect") && list.contains("/users?connect=u2"));
            assert!(list.contains("/users/u2/settings") && !list.contains("/users/u1/settings"));
            assert!(!list.contains("/pairing/"), "no panel is open");
            let pairing = context! {
                pairing => context! {
                    id => "abc", code => "K7P29QXM", qr => "<svg></svg>", urls => vec!["http://lan"],
                    fallback => true, user_id => "u2", expires_in => 120,
                },
                ..users.clone()
            };
            let code = render(locale, "users.html", pairing);
            assert!(code.contains("K7P2") && code.contains("9QXM") && code.contains("2:00"));
            assert!(code.contains("/users/u2/pairing/abc"));
            let done = render(
                locale,
                "partials/connect_result.html",
                context! {
                    status => context! { state => "connected", device_name => "Pixel 9", platform => "android" },
                    user_id => "u2", user => context! { devices => 3 },
                },
            );
            assert!(
                done.contains("Pixel 9 ") || done.contains("Pixel 9<"),
                "{done}"
            );
            // The user's device count is updated out of band.
            assert!(
                done.contains("id=\"devices-u2\" hx-swap-oob=\"true\""),
                "{done}"
            );
            let devices = context! {
                devices => vec![
                    context! { id => "d1", name => "Pixel 9", platform => "android", user_name => "Anna", paired_at => 1, last_seen_at => 2 },
                    context! { id => "d3", name => "iPhone", platform => "ios", user_name => "Mark", paired_at => 1 },
                ],
                ..base.clone()
            };
            let devices = render(locale, "devices.html", devices);
            assert_eq!(
                devices.matches("hx-post=\"/devices/").count(),
                2,
                "{devices}"
            );
            assert!(devices.contains("/devices/d1/revoke"));
        }
        let error = render(
            "ru",
            "partials/login_form.html",
            context! { error => "password_wrong" },
        );
        assert!(error.contains("Неверный пароль"), "{error}");
    }

    #[test]
    fn pages_render_in_every_locale() {
        let base = context! {
            server_name => "Home", page => "dashboard", code => "K7P29QXM",
            library_bytes => 0, user_count => 1,
            settings => context! { name => "Home", lan_url => "", wan_url => "" },
            locked => context! { name => false },
        };
        for (locale, name) in super::super::i18n::LOCALES {
            for page in [
                "dashboard.html",
                "login.html",
                "setup.html",
                "settings.html",
            ] {
                let html = render(locale, page, base.clone());
                assert!(
                    html.contains(&format!("lang=\"{locale}\"")),
                    "{locale} {page}"
                );
                assert!(html.contains(name), "{locale} {page}: no switcher entry");
            }
        }
    }

    #[test]
    fn storage_page_renders_in_both_locales() {
        let base = context! {
            server_name => "Home", page => "storage", token => "tok",
            locked => context! { name => false },
        };
        for locale in ["en", "ru"] {
            let profiles = vec![
                context! {
                    id => "s1", name => "Local", kind => "local", delivery_mode => "proxy",
                    path => "blobs", location => "/data/blobs", is_default => true, verified_at => 5,
                },
                context! {
                    id => "s2", name => "Bucket", kind => "s3", delivery_mode => "presigned",
                    endpoint => "https://s3.example.com", bucket => "books", region => "auto",
                    location => "books @ s3.example.com", is_default => false,
                    verify_error => "storage_auth_failed",
                },
            ];
            let storage = context! { profiles, data_dir => "/data", ..base.clone() };
            let page = render(locale, "storage.html", storage.clone());
            assert!(page.contains("/storage/s2/default") && !page.contains("/storage/s1/default"));
            assert!(page.contains("/storage/s2/delete") && !page.contains("/storage/s1/delete"));
            assert!(page.contains("id=\"edit-s2\"") && page.contains("value=\"books\""));
            assert!(
                !page.contains("x-init=\"$el.showModal()\""),
                "no dialog is open"
            );
            let failed = render(
                locale,
                "partials/storage_section.html",
                context! {
                    form => context! { id => "s2", kind => "s3", name => "Bucket", bucket => "" },
                    error => "storage_bucket_required", ..storage
                },
            );
            assert!(failed.contains("x-init=\"$el.showModal()\""), "{failed}");
            assert!(!failed.contains("<html"));
            let setup = render(locale, "setup.html", base.clone());
            assert!(
                setup.contains("name=\"storage_kind\"")
                    && setup.contains("name=\"storage_bucket\"")
            );
        }
    }
}
