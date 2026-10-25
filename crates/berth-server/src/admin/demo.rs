//! The demo library tab of the settings: thin wrappers over `crate::service::demo`.

use axum::{Extension, Form, extract::State, http::HeaderMap};
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    i18n::current_locale,
    pages::{Page, backup_ctx, form_error, render, render_form},
    session::AdminSession,
};
use crate::{
    AppState,
    service::{
        demo::{self, DemoPlan, Reviewer},
        languages, users,
    },
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

#[derive(Deserialize, Default)]
pub(crate) struct DemoForm {
    /// Books of the catalog, by slot, comma-separated.
    #[serde(default)]
    slots: String,
    /// `full` for whole books, else first chapters.
    #[serde(default)]
    length: String,
    /// Checkbox: `on` when the section is closed.
    #[serde(default)]
    closed: Option<String>,
    /// Ids of listeners the section is open to, comma-separated.
    #[serde(default)]
    users: String,
    /// Checkbox: `on` for a review listener.
    #[serde(default)]
    reviewer: Option<String>,
    #[serde(default)]
    reviewer_name: String,
    #[serde(default)]
    reviewer_login: String,
    #[serde(default)]
    reviewer_password: String,
}

impl DemoForm {
    fn plan(&self) -> DemoPlan {
        DemoPlan {
            slots: languages::split(&self.slots),
            full: self.length == "full",
            closed: self.closed.is_some(),
            users: languages::split(&self.users),
            reviewer: self.reviewer.is_some().then(|| Reviewer {
                name: self.reviewer_name.clone(),
                login: self.reviewer_login.clone(),
                password: self.reviewer_password.clone(),
            }),
        }
    }
}

/// The tab: the form as saved (or as sent, with `error`) and the books' progress.
async fn ctx(
    state: &AppState,
    tenant_id: &str,
    form: Option<&DemoForm>,
    error: Option<String>,
    saved: bool,
) -> anyhow::Result<Value> {
    let demo = demo::state(&state.db, tenant_id).await?;
    let listeners: Vec<Value> = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .filter(|u| u.role == "member" && demo.reviewer.as_ref().is_none_or(|r| r.0 != u.id))
        .map(|u| context! { id => u.id, name => u.name })
        .collect();
    let locale = current_locale();
    let active = !demo.books.is_empty();
    let catalog = demo::catalog();
    let (slots, full, closed, open_to) = match form {
        Some(f) => {
            let plan = f.plan();
            (plan.slots, plan.full, plan.closed, plan.users)
        }
        None if active => (
            demo.books.iter().map(|b| b.book.slot.clone()).collect(),
            demo.full,
            demo.category.as_ref().is_some_and(|c| c.closed),
            demo.category
                .as_ref()
                .map(|c| c.users.clone())
                .unwrap_or_default(),
        ),
        // A first load: the set of the admin's language, first chapters, open to all.
        None => (
            catalog
                .iter()
                .filter(|b| b.locale == if locale == "ru" { "ru" } else { "en" })
                .map(|b| b.slot.clone())
                .collect(),
            false,
            false,
            Vec::new(),
        ),
    };
    let reviewer = match (form, &demo.reviewer) {
        (Some(f), _) => context! {
            on => f.reviewer.is_some(), name => f.reviewer_name.clone(),
            login => f.reviewer_login.clone(), saved => demo.reviewer.is_some(),
        },
        (None, Some((_, name, login))) => context! {
            on => login.is_some(), name => name.clone(), login => login.clone().unwrap_or_default(),
            saved => true,
        },
        (None, None) => context! { on => false, name => "", login => "", saved => false },
    };
    let count = |locale: &str| catalog.iter().filter(|b| b.locale == locale).count();
    let counts = context! { ru => count("ru"), en => count("en") };
    // Slots of each set and megabytes of each book, for the chips and the size.
    let sets = context! {
        ru => catalog.iter().filter(|b| b.locale == "ru").map(|b| b.slot.clone()).collect::<Vec<_>>(),
        en => catalog.iter().filter(|b| b.locale == "en").map(|b| b.slot.clone()).collect::<Vec<_>>(),
    };
    let sizes: std::collections::BTreeMap<String, (u64, u64)> = catalog
        .iter()
        .map(|b| (b.slot.clone(), (b.mb_short, b.mb_full)))
        .collect();
    let busy = demo
        .books
        .iter()
        .any(|b| matches!(b.status.as_str(), "queued" | "downloading"));
    Ok(context! {
        ..backup_ctx(state, "demo"),
        ..context! {
            demo => context! {
                active, busy, slots, full, closed, open_to, listeners, reviewer, error, saved,
                books => serde(&demo.books), catalog => serde(&catalog), sizes => serde(&sizes),
                counts, sets,
                locale_names => context! { ru => "Русский", en => "English" },
                section => demo.category.as_ref().map(|c| c.name.clone()),
                ready => demo.books.iter().filter(|b| b.status == "ready").count(),
            },
        },
    })
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = ctx(&state, &session.0.tenant_id, None, None, false).await?;
    render(&state, "settings.html", ctx)
}

/// The books' progress, polled while some are loading.
pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = ctx(&state, &session.0.tenant_id, None, None, false).await?;
    render(&state, "partials/demo_books.html", ctx)
}

pub(crate) async fn apply(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<DemoForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let section = rust_i18n::t!("demo.section_name", locale = &current_locale()).into_owned();
    let ctx = match demo::apply(&state, tenant_id, &form.plan(), &section).await {
        Ok(()) => ctx(&state, tenant_id, None, None, true).await?,
        Err(err) => {
            let error = form_error(err)?;
            ctx(&state, tenant_id, Some(&form), Some(error), false).await?
        }
    };
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_demo.html",
        ctx,
    )
}

pub(crate) async fn remove(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    demo::remove(&state, tenant_id).await?;
    let ctx = ctx(&state, tenant_id, None, None, false).await?;
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_demo.html",
        ctx,
    )
}
