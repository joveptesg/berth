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
    env.add_function("t", super::i18n::translate);
    env.add_function("current_locale", super::i18n::current_locale);
    env
}

#[cfg(test)]
mod tests {
    use minijinja::context;

    use super::*;

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
    fn pages_render_in_both_locales() {
        let base = context! {
            server_name => "Home", page => "dashboard",
            settings => context! { name => "Home", lan_url => "", wan_url => "" },
            locked => context! { name => false, lan_url => false, wan_url => true },
            token => "tok", configured => true,
        };
        for (locale, dashboard, login) in [("en", "Dashboard", "Sign in"), ("ru", "Обзор", "Войти")]
        {
            let html = render(locale, "dashboard.html", base.clone());
            assert!(html.contains(dashboard), "{locale}: {html}");
            assert!(html.contains(&format!("lang=\"{locale}\"")));
            assert!(render(locale, "login.html", base.clone()).contains(login));
            assert!(render(locale, "setup.html", base.clone()).contains("tok"));
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
                    context! { id => "u1", name => "Admin", role => "owner", devices => 0 },
                    context! { id => "u2", name => "Anna", role => "member", devices => 2 },
                ],
                ..base.clone()
            };
            let list = render(locale, "users.html", users.clone());
            assert!(list.contains("/users/u2/connect") && list.contains("/users?connect=u2"));
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
