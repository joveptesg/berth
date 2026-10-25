//! The AI tab of the settings and the "fill with AI" button of a book: thin wrappers over
//! `crate::service::ai`.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::HeaderMap,
};
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    i18n::current_locale,
    library::BookForm,
    pages::{Page, backup_ctx, form_error, render, render_form},
    session::AdminSession,
};
use crate::{
    AppState,
    service::{
        ai::{self, AiInput, Subject},
        books,
    },
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct AiForm {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt: String,
    #[serde(default, skip_serializing)]
    api_key: String,
    /// Checkbox: `on` removes the saved key.
    #[serde(default, skip_serializing)]
    clear_key: Option<String>,
}

/// The tab: the connection as saved (or as sent, with `error`), the check and the run.
async fn ctx(
    state: &AppState,
    tenant_id: &str,
    form: Option<&AiForm>,
    error: Option<String>,
    saved: bool,
    check: Option<Result<(), String>>,
) -> anyhow::Result<Value> {
    let saved_settings = ai::settings(&state.db, tenant_id).await?;
    let settings = if let Some(f) = form {
        context! { base_url => &f.base_url, model => &f.model, prompt => &f.prompt }
    } else {
        context! {
            base_url => &saved_settings.base_url, model => &saved_settings.model,
            prompt => saved_settings.prompt(),
        }
    };
    let check = check.map(|r| match r {
        Ok(()) => context! { ok => true },
        Err(message) => context! { ok => false, message },
    });
    Ok(context! {
        ..backup_ctx(state, "ai"),
        ..context! {
            ai => context! {
                settings, error, saved, check,
                ready => saved_settings.ready(),
                has_key => ai::has_key(state, tenant_id).await?,
                default_prompt => ai::DEFAULT_PROMPT,
            },
            ai_run => run_ctx(state, tenant_id).await?,
        },
    })
}

async fn run_ctx(state: &AppState, tenant_id: &str) -> anyhow::Result<Value> {
    Ok(context! {
        run => serde(ai::run_state(&state.db, tenant_id).await?),
        missing => books::count_without_about(&state.db, tenant_id).await?,
        ready => ai::settings(&state.db, tenant_id).await?.ready(),
    })
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = ctx(&state, &session.0.tenant_id, None, None, false, None).await?;
    render(&state, "settings.html", ctx)
}

pub(crate) async fn save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<AiForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let input = AiInput {
        base_url: form.base_url.clone(),
        model: form.model.clone(),
        prompt: form.prompt.clone(),
        api_key: form.api_key.clone(),
        clear_key: form.clear_key.is_some(),
    };
    let ctx = match ai::save(&state, tenant_id, &input).await {
        Ok(()) => ctx(&state, tenant_id, None, None, true, None).await?,
        Err(err) => {
            let error = form_error(err)?;
            ctx(&state, tenant_id, Some(&form), Some(error), false, None).await?
        }
    };
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_ai.html",
        ctx,
    )
}

/// Sends a tiny request with the saved connection.
pub(crate) async fn check(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = ai::check(&state, tenant_id)
        .await
        .map_err(|e| format!("{e:#}"));
    let ctx = ctx(&state, tenant_id, None, None, false, Some(result)).await?;
    render_form(
        &state,
        &headers,
        "settings.html",
        "partials/settings_ai.html",
        ctx,
    )
}

/// The run's progress, polled while it goes.
pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = context! { ai_run => run_ctx(&state, &session.0.tenant_id).await? };
    render(&state, "partials/ai_run.html", ctx)
}

pub(crate) async fn start(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let error = match ai::start(&state, tenant_id, &current_locale()).await {
        Ok(()) => None,
        Err(err) => Some(form_error(err)?),
    };
    let ctx = context! { ai_run => run_ctx(&state, tenant_id).await?, run_error => error };
    render(&state, "partials/ai_run.html", ctx)
}

pub(crate) async fn stop(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    ai::stop(&state.db, tenant_id).await?;
    let ctx = context! { ai_run => run_ctx(&state, tenant_id).await? };
    render(&state, "partials/ai_run.html", ctx)
}

/// Fills the empty description and year of the book form from the AI; the admin saves.
pub(crate) async fn fill_book(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
    Form(mut form): Form<BookForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let subject = Subject {
        title: form.title.trim().to_owned(),
        authors: books::split_names(&form.authors),
        narrators: books::split_names(&form.narrators),
        series: Some(form.series.trim().to_owned()).filter(|s| !s.is_empty()),
        series_no: form.series_no.trim().replace(',', ".").parse().ok(),
        language: Some(form.language.clone()).filter(|l| !l.is_empty()),
    };
    let (notice, ai_error) =
        match ai::describe(&state, tenant_id, &subject, &current_locale()).await {
            Ok(about) => {
                let mut filled = false;
                if form.description.trim().is_empty()
                    && let Some(text) = about.description
                {
                    form.description = text;
                    filled = true;
                }
                if form.year.trim().is_empty()
                    && let Some(year) = about.year
                {
                    form.year = year.to_string();
                    filled = true;
                }
                (Some(if filled { "ai_filled" } else { "ai_unknown" }), None)
            }
            Err(err) => (None, Some(format!("{err:#}"))),
        };
    let ctx =
        super::library::book_ctx(&state, tenant_id, &id, Some(serde(&form)), None, false).await?;
    let ctx = context! { ai_notice => notice, ai_error, ..ctx };
    render(&state, "partials/book_form.html", ctx)
}
