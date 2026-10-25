//! Users with phone pairing, and devices: thin wrappers over `crate::service`.

use axum::{
    Extension, Form,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use minijinja::{Value, context};
use qrcode::{QrCode, render::svg};
use serde::Deserialize;

use super::{
    pages::{Page, form_error, render, render_form, shell},
    redirect, request_origin,
    session::AdminSession,
};
use crate::{
    AppState,
    db::now_ms,
    error::ApiError,
    service::{
        books::{self, BookQuery, PolicyFilter},
        categories, devices, groups, languages,
        pairing::{self, PairingStatus},
        settings, users,
    },
};
use berth_core::BerthError;

fn serde(value: impl serde::Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

// ── Users and phone pairing ──────────────────────────────

#[derive(Deserialize)]
pub(crate) struct UserForm {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
pub(crate) struct UsersQuery {
    /// Open the QR panel of this user (link without JavaScript, or after adding a listener).
    connect: Option<String>,
}

/// QR addresses: LAN and WAN from settings; without them, the address this page was opened at.
fn qr_urls(state: &AppState, headers: &HeaderMap) -> (Vec<String>, bool) {
    let configured = settings::server_urls(&state.config.load());
    if !configured.is_empty() {
        return (configured, false);
    }
    (vec![request_origin(headers)], true)
}

/// The QR as inline SVG; the payload is what the app expects:
/// `{"server_urls": [lan, wan], "pairing_token": "<code>"}`.
fn qr_svg(urls: &[String], code: &str) -> anyhow::Result<String> {
    let payload = serde_json::json!({ "server_urls": urls, "pairing_token": code }).to_string();
    qr(&payload)
}

/// A QR code of `payload` as inline SVG.
pub(super) fn qr(payload: &str) -> anyhow::Result<String> {
    let svg = QrCode::new(payload.as_bytes())?
        .render::<svg::Color<'_>>()
        .min_dimensions(232, 232)
        .dark_color(svg::Color("#0e1014"))
        .light_color(svg::Color("#ffffff"))
        .build();
    // Inline in HTML: drop the XML declaration.
    Ok(svg
        .find("<svg")
        .map_or(svg.as_str(), |start| &svg[start..])
        .to_owned())
}

/// Issues a code for `user_id`: the context of the QR panel.
async fn code_panel(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    user_id: &str,
) -> Result<Value, ApiError> {
    let now = now_ms();
    let code = pairing::create(&state.db, tenant_id, user_id, now).await?;
    let (urls, fallback) = qr_urls(state, headers);
    let qr = Value::from_safe_string(qr_svg(&urls, &code.code)?);
    Ok(context! {
        id => code.id,
        code => code.code,
        qr,
        urls,
        fallback,
        user_id,
        expires_in => (code.expires_at - now) / 1000,
    })
}

/// The users page on its `tab` (`people` or `groups`); `open` gets a fresh QR panel under
/// its row.
async fn users_ctx(
    state: &AppState,
    headers: &HeaderMap,
    session: &AdminSession,
    open: Option<&str>,
    tab: &str,
) -> Result<Value, ApiError> {
    let tenant_id = &session.0.tenant_id;
    let list = users::list(&state.db, tenant_id).await?;
    let open = open.filter(|id| list.iter().any(|u| u.id == *id));
    let pairing = match open {
        Some(user_id) => Some(code_panel(state, headers, tenant_id, user_id).await?),
        None => None,
    };
    let groups = groups::list(&state.db, tenant_id).await?;
    Ok(
        context! { users => serde(list), groups => serde(groups), pairing, tab, ..shell(state, "users") },
    )
}

pub(crate) async fn users_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Query(query): Query<UsersQuery>,
) -> Page {
    let ctx = users_ctx(
        &state,
        &headers,
        &session,
        query.connect.as_deref(),
        "people",
    )
    .await?;
    render(&state, "users.html", ctx)
}

/// The groups tab of the users page.
pub(crate) async fn groups_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
) -> Page {
    let ctx = users_ctx(&state, &headers, &session, None, "groups").await?;
    render(&state, "users.html", ctx)
}

/// Adds a listener and opens the QR panel for their phone right away.
pub(crate) async fn users_create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<UserForm>,
) -> Page {
    let (ctx, error) =
        match users::create_listener(&state.db, &session.0.tenant_id, &form.name).await {
            Ok(id) => {
                if !super::is_htmx(&headers) {
                    return Ok(redirect(&headers, &format!("/users?connect={id}")));
                }
                (
                    users_ctx(&state, &headers, &session, Some(&id), "people").await?,
                    None,
                )
            }
            Err(err) => (
                users_ctx(&state, &headers, &session, None, "people").await?,
                Some(form_error(err)?),
            ),
        };
    let name = error.as_ref().map(|_| form.name);
    let ctx = context! { error, name, ..ctx };
    render_form(
        &state,
        &headers,
        "users.html",
        "partials/users_section.html",
        ctx,
    )
}

/// A listener's login for apps and the books they see.
#[derive(Deserialize)]
pub(crate) struct UserSettingsForm {
    /// Checkbox: `on` when apps may sign in with the login.
    #[serde(default)]
    login_on: Option<String>,
    #[serde(default)]
    login: String,
    /// Empty keeps the saved password.
    #[serde(default)]
    password: String,
    /// `all` or `selected`.
    #[serde(default)]
    access: String,
    /// Ids of the picked books, comma-separated.
    #[serde(default)]
    books: String,
    /// Language codes the user sees, comma-separated; empty for all.
    #[serde(default)]
    languages: String,
    /// Ids of the sections given, comma-separated.
    #[serde(default)]
    categories: String,
    /// Ids of the groups the user is in, comma-separated.
    #[serde(default)]
    groups: String,
    /// A child's date of birth, `YYYY-MM-DD`; empty for an adult.
    #[serde(default)]
    birth_date: String,
    /// Checkbox: `on` when a child sees books without a rating.
    #[serde(default)]
    unrated_on: Option<String>,
}

/// Comma-separated ids, without blanks.
fn ids(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

impl UserSettingsForm {
    fn picked(&self) -> Vec<String> {
        ids(&self.books)
    }
}

/// The settings dialog of a listener: as saved, or as `form` sent with its `error`.
async fn settings_ctx(
    state: &AppState,
    tenant_id: &str,
    user_id: &str,
    form: Option<&UserSettingsForm>,
    error: Option<String>,
) -> Result<Value, ApiError> {
    let user = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .find(|u| u.id == user_id && u.role == "member")
        .ok_or(BerthError::NotFound)?;
    let library = books::default_library_id(&state.db, tenant_id).await?;
    let list = books::list(
        &state.db,
        tenant_id,
        &PolicyFilter::admin(),
        &library,
        &BookQuery::default(),
    )
    .await?;
    let picked: Vec<String> = match form {
        Some(form) => form.picked(),
        None => users::access(&state.db, tenant_id, user_id)
            .await?
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_default(),
    };
    let given = match form {
        Some(form) => languages::split(&form.categories),
        None => categories::given(&state.db, tenant_id, user_id).await?,
    };
    let sections = categories::entries(&state.db, tenant_id).await?;
    let in_groups = match form {
        Some(form) => ids(&form.groups),
        None => groups::of_user(&state.db, tenant_id, user_id).await?,
    };
    let birth_date = form.map_or(user.birth_date.clone().unwrap_or_default(), |f| {
        f.birth_date.trim().to_owned()
    });
    // A new child hides books without a rating until the admin allows them.
    let unrated_on = form.map_or(user.birth_date.is_some() && user.unrated_allowed, |f| {
        f.unrated_on.is_some()
    });
    let ceiling = books::rating_ceiling(&birth_date, now_ms());
    Ok(context! {
        user_settings => context! {
            groups => serde(groups::list(&state.db, tenant_id).await?),
            in_groups,
            birth_date,
            unrated_on,
            ceiling,
            ratings => books::RATINGS.to_vec(),
            access => form.map_or(user.book_access.clone(), |f| f.access.clone()),
            login_on => form.map_or(user.login.is_some(), |f| f.login_on.is_some()),
            login => form.map_or(user.login.clone().unwrap_or_default(), |f| f.login.clone()),
            has_password => user.login.is_some(),
            spoken => form.map_or(user.languages.clone(), |f| languages::split(&f.languages)),
            user => serde(user),
            books => serde(list),
            picked,
            languages => serde(languages::list(&state.db, tenant_id).await?),
            sections => serde(sections),
            given,
            min_password => users::MIN_APP_PASSWORD_LEN,
            error,
        }
    })
}

/// The settings dialog, loaded into the users page.
pub(crate) async fn user_settings(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(user_id): Path<String>,
) -> Page {
    let ctx = settings_ctx(&state, &session.0.tenant_id, &user_id, None, None).await?;
    render(&state, "partials/user_settings.html", ctx)
}

/// Saves a listener's login and book access; the dialog comes back open with the error.
pub(crate) async fn user_settings_save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Form(form): Form<UserSettingsForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let saved = async {
        if form.login_on.is_some() {
            users::set_login(&state.db, tenant_id, &user_id, &form.login, &form.password).await?;
        } else {
            users::clear_login(&state.db, tenant_id, &user_id).await?;
        }
        let access = users::AccessInput {
            books: (form.access == "selected").then(|| form.picked()),
            categories: ids(&form.categories),
            languages: languages::split(&form.languages),
            groups: ids(&form.groups),
            birth_date: Some(form.birth_date.clone()),
            unrated_allowed: form.unrated_on.is_some(),
        };
        users::set_access(&state.db, tenant_id, &user_id, &access).await
    }
    .await;
    let ctx = users_ctx(&state, &headers, &session, None, "people").await?;
    let ctx = match saved {
        Ok(()) => {
            if !super::is_htmx(&headers) {
                return Ok(redirect(&headers, "/users"));
            }
            ctx
        }
        Err(err) => {
            let error = Some(form_error(err)?);
            let settings = settings_ctx(&state, tenant_id, &user_id, Some(&form), error).await?;
            context! { ..settings, ..ctx }
        }
    };
    render_form(
        &state,
        &headers,
        "users.html",
        "partials/users_section.html",
        ctx,
    )
}

// ── Groups ───────────────────────────────────────────────

/// A group's name, the books it sees and its members.
#[derive(Deserialize)]
pub(crate) struct GroupForm {
    #[serde(default)]
    name: String,
    /// `all` or `selected`.
    #[serde(default)]
    access: String,
    /// Ids of the picked books, comma-separated.
    #[serde(default)]
    books: String,
    /// Language codes the group sees, comma-separated; empty for all.
    #[serde(default)]
    languages: String,
    /// Ids of the sections given, comma-separated.
    #[serde(default)]
    categories: String,
    /// Ids of the listeners in the group, comma-separated.
    #[serde(default)]
    members: String,
}

impl GroupForm {
    fn input(&self) -> groups::GroupInput {
        groups::GroupInput {
            name: self.name.clone(),
            books: (self.access == "selected").then(|| ids(&self.books)),
            categories: ids(&self.categories),
            languages: languages::split(&self.languages),
            members: ids(&self.members),
        }
    }
}

/// The dialog of a group (`id` empty for a new one): as saved, or as `form` sent with its
/// `error`.
async fn group_ctx(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    form: Option<&GroupForm>,
    error: Option<String>,
) -> Result<Value, ApiError> {
    let saved = if id.is_empty() {
        None
    } else {
        let group = groups::list(&state.db, tenant_id)
            .await?
            .into_iter()
            .find(|g| g.id == id)
            .ok_or(BerthError::NotFound)?;
        Some(group)
    };
    let library = books::default_library_id(&state.db, tenant_id).await?;
    let list = books::list(
        &state.db,
        tenant_id,
        &PolicyFilter::admin(),
        &library,
        &BookQuery::default(),
    )
    .await?;
    let picked = match (form, &saved) {
        (Some(form), _) => ids(&form.books),
        (None, Some(_)) => groups::picked(&state.db, tenant_id, id).await?,
        (None, None) => Vec::new(),
    };
    let listeners: Vec<Value> = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .filter(|u| u.role == "member")
        .map(|u| context! { id => u.id, name => u.name })
        .collect();
    let (name, access, spoken, given, members) = match (form, saved) {
        (Some(f), _) => (
            f.name.clone(),
            f.access.clone(),
            languages::split(&f.languages),
            ids(&f.categories),
            ids(&f.members),
        ),
        (None, Some(g)) => (g.name, g.book_access, g.languages, g.categories, g.members),
        (None, None) => (
            String::new(),
            "all".to_owned(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    };
    Ok(context! {
        group_settings => context! {
            id,
            name,
            access,
            picked,
            spoken,
            given,
            members,
            listeners,
            books => serde(list),
            languages => serde(languages::list(&state.db, tenant_id).await?),
            sections => serde(categories::entries(&state.db, tenant_id).await?),
            error,
        }
    })
}

/// The empty dialog of a new group.
pub(crate) async fn group_new(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = group_ctx(&state, &session.0.tenant_id, "", None, None).await?;
    render(&state, "partials/group_settings.html", ctx)
}

/// The dialog of a group as saved.
pub(crate) async fn group_settings(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let ctx = group_ctx(&state, &session.0.tenant_id, &id, None, None).await?;
    render(&state, "partials/group_settings.html", ctx)
}

/// The users section after a group action, or with the group dialog open on its error.
async fn group_answer(
    state: &AppState,
    headers: &HeaderMap,
    session: &AdminSession,
    id: &str,
    form: &GroupForm,
    saved: berth_core::Result<()>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let ctx = users_ctx(state, headers, session, None, "groups").await?;
    let ctx = match saved {
        Ok(()) => {
            if !super::is_htmx(headers) {
                return Ok(redirect(headers, "/users/groups"));
            }
            ctx
        }
        Err(err) => {
            let error = Some(form_error(err)?);
            let dialog = group_ctx(state, tenant_id, id, Some(form), error).await?;
            context! { ..dialog, ..ctx }
        }
    };
    render_form(
        state,
        headers,
        "users.html",
        "partials/users_section.html",
        ctx,
    )
}

pub(crate) async fn group_create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<GroupForm>,
) -> Page {
    let saved = groups::create(&state.db, &session.0.tenant_id, &form.input())
        .await
        .map(|_| ());
    group_answer(&state, &headers, &session, "", &form, saved).await
}

pub(crate) async fn group_save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<GroupForm>,
) -> Page {
    let saved = groups::update(&state.db, &session.0.tenant_id, &id, &form.input()).await;
    group_answer(&state, &headers, &session, &id, &form, saved).await
}

pub(crate) async fn group_delete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    groups::delete(&state.db, &session.0.tenant_id, &id).await?;
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, "/users/groups"));
    }
    let ctx = users_ctx(&state, &headers, &session, None, "groups").await?;
    render(&state, "partials/users_section.html", ctx)
}

/// QR panel under a user's row.
pub(crate) async fn user_connect(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Page {
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, &format!("/users?connect={user_id}")));
    }
    let pairing = code_panel(&state, &headers, &session.0.tenant_id, &user_id).await?;
    render(&state, "partials/connect_code.html", context! { pairing })
}

/// Polled by the QR panel every 2 s: 204 while waiting, then the final panel and, once the
/// phone is connected, the user's new device count.
pub(crate) async fn pairing_status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path((user_id, id)): Path<(String, String)>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let status = pairing::status(&state.db, tenant_id, &id, now_ms()).await?;
    if matches!(status, PairingStatus::Waiting { .. }) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let user = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .find(|u| u.id == user_id);
    render(
        &state,
        "partials/connect_result.html",
        context! { status => serde(status), user_id, user => serde(user) },
    )
}

// ── Devices ──────────────────────────────────────────────

pub(crate) async fn devices_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let list = devices::list(&state.db, &session.0.tenant_id).await?;
    render(
        &state,
        "devices.html",
        context! {
            devices => serde(list), ..shell(&state, "devices")
        },
    )
}

pub(crate) async fn devices_revoke(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    devices::revoke(&state.db, tenant_id, &id, now_ms()).await?;
    if !super::is_htmx(&headers) {
        return Ok(redirect(&headers, "/devices"));
    }
    // The row is swapped for nothing: revoked devices are not listed.
    Ok(().into_response())
}
