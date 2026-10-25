//! Descriptions and years of books from an AI model behind any OpenAI-compatible API
//! (`POST {base_url}/chat/completions`): `OpenAI`, `OpenRouter`, `DeepSeek`, a local `Ollama`.
//!
//! Only empty fields are filled, so the admin's own text is never replaced. A run over the
//! library is a chain of `ai_describe` jobs, one book each; its progress is a tenant setting.

use std::time::Duration;

use anyhow::Context;
use berth_core::BerthError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::books::{self, Book, MAX_DESCRIPTION_LEN, PolicyFilter, YEARS};
use crate::{
    AppState,
    db::{self, now_ms},
    jobs,
};

type Result<T> = std::result::Result<T, BerthError>;

const SETTINGS_KEY: &str = "ai";
const API_KEY_KEY: &str = "ai_key";
const RUN_KEY: &str = "ai_run";
const MAX_PROMPT_LEN: usize = 4000;

/// The instruction the admin may change; `{language}` becomes the book's language.
pub const DEFAULT_PROMPT: &str = "You are a bibliographer. You are given the metadata of an audiobook. \
Write `description`: 2–4 sentences in {language} about what the book is about — no spoilers, \
no reviews or praise. Give `year`: the year the original work was first published. \
If you do not confidently recognize the book, use null for that field. Never invent facts.";

/// Added after the admin's instruction, so a changed prompt cannot break the answer format.
const FORMAT: &str = "Answer with a single JSON object and nothing else: \
{\"description\": string or null, \"year\": integer or null}.";

/// The connection as the admin set it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiSettings {
    /// API root, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model name, e.g. `gpt-4o-mini`.
    pub model: String,
    /// The admin's instruction; empty for [`DEFAULT_PROMPT`].
    #[serde(default)]
    pub prompt: String,
}

impl AiSettings {
    /// The instruction in use.
    #[must_use]
    pub fn prompt(&self) -> &str {
        if self.prompt.trim().is_empty() {
            DEFAULT_PROMPT
        } else {
            &self.prompt
        }
    }

    /// Set up enough to ask.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.base_url.is_empty() && !self.model.is_empty()
    }
}

/// The settings form.
#[derive(Debug, Clone, Default)]
pub struct AiInput {
    /// API root.
    pub base_url: String,
    /// Model.
    pub model: String,
    /// Instruction; the default text or empty means the default.
    pub prompt: String,
    /// New key; empty keeps the saved one.
    pub api_key: String,
    /// Removes the saved key (a local model needs none).
    pub clear_key: bool,
}

/// The connection settings.
///
/// # Errors
/// Database failures.
pub async fn settings(db: &sqlx::SqlitePool, tenant_id: &str) -> anyhow::Result<AiSettings> {
    Ok(db::tenant_setting(db, tenant_id, SETTINGS_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

/// Whether an API key is saved.
///
/// # Errors
/// Database failures.
pub async fn has_key(state: &AppState, tenant_id: &str) -> anyhow::Result<bool> {
    Ok(api_key(state, tenant_id).await?.is_some())
}

async fn api_key(state: &AppState, tenant_id: &str) -> anyhow::Result<Option<String>> {
    db::tenant_secret(&state.db, &state.secret, tenant_id, API_KEY_KEY).await
}

/// Saves the connection.
///
/// # Errors
/// `Validation` with `ai_url_invalid`, `ai_model_required` or `ai_prompt_too_long`.
pub async fn save(state: &AppState, tenant_id: &str, input: &AiInput) -> Result<()> {
    let base_url = input.base_url.trim().trim_end_matches('/');
    let url_ok = url::Url::parse(base_url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    if !url_ok {
        return Err(BerthError::Validation("ai_url_invalid".into()));
    }
    let model = input.model.trim();
    if model.is_empty() {
        return Err(BerthError::Validation("ai_model_required".into()));
    }
    let prompt = input.prompt.trim();
    if prompt.chars().count() > MAX_PROMPT_LEN {
        return Err(BerthError::Validation("ai_prompt_too_long".into()));
    }
    let settings = AiSettings {
        base_url: base_url.to_owned(),
        model: model.to_owned(),
        // The default is kept as empty, so a better default in a later release applies.
        prompt: if prompt == DEFAULT_PROMPT { "" } else { prompt }.to_owned(),
    };
    let mut tx = state.db.begin().await.context("begin")?;
    db::put_tenant_setting(&mut tx, tenant_id, SETTINGS_KEY, &json!(settings))
        .await
        .context("save ai settings")?;
    let key = input.api_key.trim();
    if input.clear_key {
        db::put_tenant_secret(&mut tx, &state.secret, tenant_id, API_KEY_KEY, None).await?;
    } else if !key.is_empty() {
        db::put_tenant_secret(&mut tx, &state.secret, tenant_id, API_KEY_KEY, Some(key)).await?;
    }
    tx.commit().await.context("commit ai settings")?;
    Ok(())
}

/// What the model said about a book; `None` for what it does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct About {
    /// A few sentences.
    pub description: Option<String>,
    /// Year of first publication.
    pub year: Option<i64>,
}

/// The book the model is asked about.
#[derive(Debug, Clone, Default)]
pub struct Subject {
    /// Title.
    pub title: String,
    /// Authors.
    pub authors: Vec<String>,
    /// Narrators.
    pub narrators: Vec<String>,
    /// Series.
    pub series: Option<String>,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Language code of the book.
    pub language: Option<String>,
}

impl From<&Book> for Subject {
    fn from(book: &Book) -> Self {
        Self {
            title: book.title.clone(),
            authors: book.authors.clone(),
            narrators: book.narrators.clone(),
            series: book.series.clone(),
            series_no: book.series_no,
            language: book.language.clone(),
        }
    }
}

impl Subject {
    fn message(&self, language: &str) -> String {
        let mut lines = vec![format!("Title: {}", self.title)];
        if !self.authors.is_empty() {
            lines.push(format!("Author: {}", self.authors.join(", ")));
        }
        if let Some(series) = &self.series {
            match self.series_no {
                Some(no) => lines.push(format!("Series: {series} #{no}")),
                None => lines.push(format!("Series: {series}")),
            }
        }
        if !self.narrators.is_empty() {
            lines.push(format!("Narrator: {}", self.narrators.join(", ")));
        }
        lines.push(format!("Language: {language}"));
        lines.join("\n")
    }
}

/// English name of a language code, as models understand it best; unknown codes pass as is.
fn language_name(code: &str) -> &str {
    match code {
        "ru" => "Russian",
        "en" => "English",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" => "Portuguese",
        "uk" => "Ukrainian",
        "be" => "Belarusian",
        "pl" => "Polish",
        "cs" => "Czech",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "fi" => "Finnish",
        "tr" => "Turkish",
        "kk" => "Kazakh",
        "zh" => "Chinese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "ar" => "Arabic",
        "he" => "Hebrew",
        other => other,
    }
}

/// Asks the model about a book: in its language, else in `fallback_language` (a code).
///
/// # Errors
/// Not set up, network failures, an error from the API.
pub async fn describe(
    state: &AppState,
    tenant_id: &str,
    subject: &Subject,
    fallback_language: &str,
) -> anyhow::Result<About> {
    let settings = settings(&state.db, tenant_id).await?;
    anyhow::ensure!(settings.ready(), "the AI connection is not set up");
    let key = api_key(state, tenant_id).await?;
    let language = language_name(subject.language.as_deref().unwrap_or(fallback_language));
    let system = format!(
        "{}\n\n{FORMAT}",
        settings.prompt().replace("{language}", language)
    );
    let answer = complete(
        &settings,
        key.as_deref(),
        &system,
        &subject.message(language),
    )
    .await?;
    Ok(parse(&answer))
}

/// Checks the connection with a tiny request.
///
/// # Errors
/// What went wrong, for the admin.
pub async fn check(state: &AppState, tenant_id: &str) -> anyhow::Result<()> {
    let settings = settings(&state.db, tenant_id).await?;
    anyhow::ensure!(settings.ready(), "the AI connection is not set up");
    let key = api_key(state, tenant_id).await?;
    complete(
        &settings,
        key.as_deref(),
        "Answer with a single JSON object and nothing else.",
        "Reply with {\"ok\": true}.",
    )
    .await
    .map(drop)
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        // Local models on a small server can take a while.
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("Berth/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// One chat completion; the text of the answer.
async fn complete(
    settings: &AiSettings,
    key: Option<&str>,
    system: &str,
    user: &str,
) -> anyhow::Result<String> {
    let url = format!("{}/chat/completions", settings.base_url);
    let mut body = json!({
        "model": settings.model,
        "temperature": 0.2,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "response_format": { "type": "json_object" },
    });
    let http = client()?;
    loop {
        let mut request = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.context("no answer from the AI API")?;
        let status = response.status();
        let bytes = response.bytes().await.context("read the AI answer")?;
        // Some providers do not know `response_format`: ask again without it.
        if status == reqwest::StatusCode::BAD_REQUEST
            && let Some(fields) = body.as_object_mut()
            && fields.remove("response_format").is_some()
        {
            continue;
        }
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            let snippet: String = text.chars().take(300).collect();
            anyhow::bail!("the AI API answered {status}: {snippet}");
        }
        let value: Value = serde_json::from_slice(&bytes).context("the AI answer is not JSON")?;
        return value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .context("no text in the AI answer");
    }
}

/// The JSON object in the answer, even wrapped in prose or a code fence.
fn parse(answer: &str) -> About {
    let object = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<Value>(&answer[start..=end]).ok())
        .unwrap_or_default();
    let description = object["description"]
        .as_str()
        .map(str::trim)
        .filter(|d| !d.is_empty() && d.chars().count() <= MAX_DESCRIPTION_LEN)
        .map(str::to_owned);
    let year = object["year"]
        .as_i64()
        .or_else(|| object["year"].as_str()?.trim().parse().ok())
        .filter(|y| YEARS.contains(y));
    About { description, year }
}

// ── A run over the library ───────────────────────────────

/// Progress of filling the library.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Run {
    /// Still going.
    pub running: bool,
    /// Books that lacked a description or a year at the start.
    pub total: i64,
    /// Books asked about.
    pub done: i64,
    /// Books that got something.
    pub filled: i64,
    /// Books the model did not know.
    pub missed: i64,
    /// Why the run stopped early.
    pub error: Option<String>,
    /// When it finished or stopped.
    pub finished_at: Option<i64>,
    /// Language for books without one.
    #[serde(default)]
    pub locale: String,
}

/// The last run.
///
/// # Errors
/// Database failures.
pub async fn run_state(db: &sqlx::SqlitePool, tenant_id: &str) -> anyhow::Result<Run> {
    Ok(db::tenant_setting(db, tenant_id, RUN_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

async fn put_run(db: &sqlx::SqlitePool, tenant_id: &str, run: &Run) -> anyhow::Result<()> {
    let mut conn = db.acquire().await?;
    db::put_tenant_setting(&mut conn, tenant_id, RUN_KEY, &json!(run)).await?;
    Ok(())
}

/// Starts filling the books that lack a description or a year; `locale` is the language
/// for books without one. A run already going is left alone.
///
/// # Errors
/// `Validation("ai_not_set_up")`, database failures.
pub async fn start(state: &AppState, tenant_id: &str, locale: &str) -> Result<()> {
    if !settings(&state.db, tenant_id).await?.ready() {
        return Err(BerthError::Validation("ai_not_set_up".into()));
    }
    if run_state(&state.db, tenant_id).await?.running {
        return Ok(());
    }
    let run = Run {
        running: true,
        total: books::count_without_about(&state.db, tenant_id).await?,
        locale: locale.to_owned(),
        ..Run::default()
    };
    put_run(&state.db, tenant_id, &run).await?;
    let mut conn = state.db.acquire().await.context("acquire")?;
    jobs::enqueue(
        &mut conn,
        tenant_id,
        jobs::AI_DESCRIBE,
        &json!({ "after": "" }),
    )
    .await
    .context("enqueue")?;
    state.wake_jobs();
    Ok(())
}

/// Stops the run after the book being asked about.
///
/// # Errors
/// Database failures.
pub async fn stop(db: &sqlx::SqlitePool, tenant_id: &str) -> anyhow::Result<()> {
    let mut run = run_state(db, tenant_id).await?;
    if run.running {
        run.running = false;
        run.finished_at = Some(now_ms());
        put_run(db, tenant_id, &run).await?;
    }
    Ok(())
}

/// One step of a run: the next book after `payload.after`, then the step after it.
/// A failed request ends the run with its error instead of retrying: a wrong key or model
/// would fail for every book.
///
/// # Errors
/// Database failures.
pub(crate) async fn run_job(
    state: &AppState,
    tenant_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let mut run = run_state(&state.db, tenant_id).await?;
    if !run.running {
        return Ok(());
    }
    let after = payload["after"].as_str().unwrap_or_default();
    let Some(id) = books::next_without_about(&state.db, tenant_id, after).await? else {
        run.running = false;
        run.finished_at = Some(now_ms());
        return put_run(&state.db, tenant_id, &run).await;
    };
    let book = books::get(&state.db, tenant_id, &PolicyFilter::admin(), &id)
        .await
        .map_err(anyhow::Error::from)?
        .book;
    let result = describe(state, tenant_id, &Subject::from(&book), &run.locale).await;
    // The admin may have stopped the run while the model was thinking.
    let stopped = !run_state(&state.db, tenant_id).await?.running;
    run.done += 1;
    match result {
        Ok(about) => {
            let filled = books::fill_about(
                &state.db,
                tenant_id,
                &id,
                about.description.as_deref(),
                about.year,
            )
            .await?;
            if filled {
                run.filled += 1;
            } else {
                run.missed += 1;
            }
        }
        Err(err) => {
            tracing::warn!(book = %id, error = %format!("{err:#}"), "AI description failed");
            run.running = false;
            run.error = Some(format!("{err:#}"));
            run.finished_at = Some(now_ms());
            return put_run(&state.db, tenant_id, &run).await;
        }
    }
    if stopped {
        run.running = false;
        run.finished_at = Some(now_ms());
        return put_run(&state.db, tenant_id, &run).await;
    }
    put_run(&state.db, tenant_id, &run).await?;
    let mut conn = state.db.acquire().await?;
    jobs::enqueue(
        &mut conn,
        tenant_id,
        jobs::AI_DESCRIBE,
        &json!({ "after": id }),
    )
    .await?;
    state.wake_jobs();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_answer_object() {
        assert_eq!(
            parse("```json\n{\"description\": \" A tale. \", \"year\": 1865}\n```"),
            About {
                description: Some("A tale.".into()),
                year: Some(1865)
            }
        );
        assert_eq!(
            parse("{\"description\": null, \"year\": \"1902\"}"),
            About {
                description: None,
                year: Some(1902)
            }
        );
        assert_eq!(
            parse("{\"description\": \"\", \"year\": 3000}"),
            About::default()
        );
        assert_eq!(parse("I do not know this book."), About::default());
    }

    #[test]
    fn the_message_names_the_book() {
        let subject = Subject {
            title: "Dune".into(),
            authors: vec!["Frank Herbert".into()],
            series: Some("Dune".into()),
            series_no: Some(1.0),
            ..Subject::default()
        };
        assert_eq!(
            subject.message("English"),
            "Title: Dune\nAuthor: Frank Herbert\nSeries: Dune #1\nLanguage: English"
        );
        assert_eq!(language_name("ru"), "Russian");
        assert_eq!(language_name("eo"), "eo");
    }
}
