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

            let users = context! {
                users => vec![
                    context! { id => "u1", name => "Admin", role => "owner", devices => 0 },
                    context! { id => "u2", name => "Anna", role => "member", devices => 2 },
                ],
                ..base.clone()
            };
            assert!(render(locale, "users.html", users.clone()).contains("/connect?user=u2"));
            let connect = render(locale, "connect.html", users.clone());
            // The first listener is preselected, not the owner.
            assert!(connect.contains("user: 'u2'"), "{connect}");
            let pairing = context! {
                pairing => context! {
                    id => "abc", code => "K7P29QXM", qr => "<svg></svg>", urls => vec!["http://lan"],
                    fallback => true, user_id => "u2", user_name => "Anna", expires_in => 120,
                },
                ..users.clone()
            };
            let code = render(locale, "connect.html", pairing);
            assert!(code.contains("K7P2") && code.contains("9QXM") && code.contains("2:00"));
            assert!(code.contains("/connect/status/abc"));
            let done = render(
                locale,
                "partials/connect_result.html",
                context! { status => context! { state => "connected", device_name => "Pixel 9", platform => "android" } },
            );
            assert!(
                done.contains("Pixel 9 ") || done.contains("Pixel 9<"),
                "{done}"
            );
            let devices = context! {
                devices => vec![
                    context! { id => "d1", name => "Pixel 9", platform => "android", user_name => "Anna", paired_at => 1, last_seen_at => 2, revoked_at => () },
                    context! { id => "d2", name => "Chrome on Windows", platform => "web", user_name => "Admin", paired_at => 1, revoked_at => () },
                    context! { id => "d3", name => "Old phone", platform => "ios", user_name => "Anna", paired_at => 1, revoked_at => 3 },
                ],
                current => "d2",
                ..base.clone()
            };
            let devices = render(locale, "devices.html", devices);
            // Only the active phone can be revoked: not this browser, not the revoked one.
            assert_eq!(
                devices.matches("hx-post=\"/devices/").count(),
                1,
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
}
