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
        }
        let error = render(
            "ru",
            "partials/login_form.html",
            context! { error => "password_wrong" },
        );
        assert!(error.contains("Неверный пароль"), "{error}");
    }
}
