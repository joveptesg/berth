//! Every six hours: is there a release newer than this server? The admin shows a mark next to
//! the version. Only the public release feed is fetched; nothing about the server is sent.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::AppState;

const INTERVAL: Duration = Duration::from_hours(6);
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// A release newer than the running server.
#[derive(Debug, Clone, Serialize)]
pub struct Release {
    /// Version without the `v`, e.g. `1.0.17`.
    pub version: String,
    /// Release notes page.
    pub url: String,
    /// Image tag that follows this release (`1` in `docker-compose.yml`), set when the major
    /// version changed and the tag has to be changed by hand.
    pub major_tag: Option<u64>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

/// Starts the check; it runs right away and then every six hours.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        loop {
            interval.tick().await;
            run(&state).await;
        }
    });
}

/// Checks once, or forgets the found release when checking is off. A failed check keeps
/// what was found before.
pub async fn run(state: &AppState) {
    let config = state.config.load();
    if !config.check_updates {
        state.latest_release.store(None);
        return;
    }
    match fetch(config.release_source.as_str()).await {
        Ok(latest) => state
            .latest_release
            .store(newer(CURRENT, latest).map(Arc::new)),
        Err(err) => tracing::debug!(error = %format!("{err:#}"), "update check failed"),
    }
}

async fn fetch(url: &str) -> anyhow::Result<GithubRelease> {
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Berth/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    serde_json::from_str(&body).context("release feed")
}

/// The release, when it is newer than `current`.
fn newer(current: &str, latest: GithubRelease) -> Option<Release> {
    let version = latest.tag_name.trim_start_matches('v');
    let (now, next) = (parse(current)?, parse(version)?);
    (next > now).then(|| Release {
        version: version.to_owned(),
        url: latest.html_url,
        major_tag: (next.0 > now.0).then_some(next.0),
    })
}

/// `X.Y.Z`; pre-releases and other shapes are not offered.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(str::parse);
    let parsed = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            html_url: format!("https://github.com/joveptesg/berth/releases/tag/{tag}"),
        }
    }

    #[test]
    fn offers_only_newer_releases() {
        let found = newer("1.0.9", release("v1.0.10")).unwrap();
        assert_eq!(found.version, "1.0.10");
        assert_eq!(found.major_tag, None);
        assert!(found.url.ends_with("/v1.0.10"));

        assert!(newer("1.0.10", release("v1.0.10")).is_none());
        assert!(newer("1.1.0", release("v1.0.10")).is_none());
        assert!(newer("1.0.0", release("v1.1.0-rc.1")).is_none());
        assert!(newer("1.0.0", release("nightly")).is_none());

        assert_eq!(
            newer("1.4.2", release("v2.0.0")).unwrap().major_tag,
            Some(2)
        );
    }
}
