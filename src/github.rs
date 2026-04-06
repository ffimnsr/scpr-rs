use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use std::time::Duration;
use tracing::{debug, info, warn};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: usize = 3;
const MAX_DOWNLOAD_SIZE_BYTES: u64 = 500 * 1024 * 1024;

/// A GitHub release returned by the releases API.
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
pub struct RepoMetadata {
    pub default_branch: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitTreeResponse {
    pub tree: Vec<GitTreeEntry>,
}

#[derive(Debug, Deserialize)]
pub struct GitTreeEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
}

/// A single asset attached to a GitHub release.
#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

/// Thin wrapper around [`reqwest::Client`] for GitHub API calls.
#[derive(Clone)]
pub struct GithubClient {
    client: reqwest::Client,
}

impl GithubClient {
    /// Build a new client that identifies itself as `scpr/<version>`.
    ///
    /// If the `GITHUB_TOKEN` environment variable is set, it is sent as a
    /// `Bearer` token on every request, raising the API rate limit from 60
    /// to 5 000 requests per hour and enabling access to private repositories.
    pub fn new(version: &str) -> Result<Self> {
        let mut default_headers = HeaderMap::new();
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("GITHUB_TOKEN contains invalid characters")?;
            value.set_sensitive(true);
            default_headers.insert(AUTHORIZATION, value);
            info!("GITHUB_TOKEN detected — using authenticated GitHub API requests");
        }
        let client = reqwest::Client::builder()
            .user_agent(format!("scpr/{version}"))
            .timeout(REQUEST_TIMEOUT)
            .default_headers(default_headers)
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self { client })
    }

    /// Fetch the latest release metadata for `owner/repo`.
    pub async fn get_latest_release(&self, owner: &str, repo: &str) -> Result<Release> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
        debug!("Fetching latest release: {url}");

        let response = self.get_with_retries(&url, "GitHub API request").await?;

        let release: Release = response
            .json()
            .await
            .context("Failed to parse GitHub release response")?;

        debug!("Latest release tag: {}", release.tag_name);
        Ok(release)
    }

    pub async fn get_repo_metadata(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<RepoMetadata> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}");
        debug!("Fetching repo metadata: {url}");
        let response = self.get_with_retries(&url, "GitHub API request").await?;
        response
            .json()
            .await
            .context("Failed to parse GitHub repo metadata")
    }

    pub async fn get_git_tree(
        &self,
        owner: &str,
        repo: &str,
        r#ref: &str,
    ) -> Result<GitTreeResponse> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/git/trees/{ref}?recursive=1",
            ref = r#ref
        );
        debug!("Fetching git tree: {url}");
        let response = self.get_with_retries(&url, "GitHub API request").await?;
        response
            .json()
            .await
            .context("Failed to parse GitHub git tree response")
    }

    pub async fn download_text(&self, url: &str) -> Result<String> {
        debug!("Downloading text: {url}");
        let response = self.get_with_retries(url, "text download request").await?;
        response
            .text()
            .await
            .context("Failed to read text response")
    }

    /// Fetch a specific release by tag for `owner/repo`.
    pub async fn get_release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Release> {
        let normalized_tag = normalize_tag(tag);
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/tags/{normalized_tag}"
        );
        debug!("Fetching release by tag: {url}");

        let response = self.get_with_retries(&url, "GitHub API request").await?;

        let release: Release = response
            .json()
            .await
            .context("Failed to parse GitHub release response")?;

        debug!("Resolved release tag: {}", release.tag_name);
        Ok(release)
    }

    /// Download `url` while displaying a progress bar.
    ///
    /// `expected_size` is used to size the progress bar; pass `0` if unknown.
    pub async fn download_asset(&self, url: &str, expected_size: u64) -> Result<Vec<u8>> {
        debug!("Downloading asset: {url}");

        let response = self.get_with_retries(url, "asset download request").await?;

        let total = response.content_length().unwrap_or(expected_size);
        if total > MAX_DOWNLOAD_SIZE_BYTES {
            return Err(anyhow!(
                "Refusing to download asset larger than {} bytes (reported size: {} bytes)",
                MAX_DOWNLOAD_SIZE_BYTES,
                total
            ));
        }

        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] \
                     [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
                )
                .unwrap()
                .progress_chars("#>-"),
        );

        let mut data: Vec<u8> = Vec::with_capacity(total as usize);
        let mut stream = response.bytes_stream();
        let mut downloaded = 0_u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read response chunk")?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            if downloaded > MAX_DOWNLOAD_SIZE_BYTES {
                pb.finish_and_clear();
                return Err(anyhow!(
                    "Refusing to download asset larger than {} bytes",
                    MAX_DOWNLOAD_SIZE_BYTES
                ));
            }
            pb.inc(chunk.len() as u64);
            data.extend_from_slice(&chunk);
        }

        pb.finish_with_message("Download complete");
        Ok(data)
    }

    async fn get_with_retries(
        &self,
        url: &str,
        context: &str,
    ) -> Result<reqwest::Response> {
        let mut last_error = None;

        for attempt in 1..=MAX_RETRIES {
            match self.client.get(url).send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let should_retry =
                        should_retry_status(status) && attempt < MAX_RETRIES;
                    let error = build_http_error(response, url, context);
                    if should_retry {
                        warn!(
                            "{context} failed with {status} on attempt {attempt}/{MAX_RETRIES}; retrying"
                        );
                        tokio::time::sleep(retry_delay(attempt)).await;
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
                Err(err) => {
                    let retryable =
                        (err.is_timeout() || err.is_connect()) && attempt < MAX_RETRIES;
                    if retryable {
                        warn!(
                            "{context} failed on attempt {attempt}/{MAX_RETRIES}: {err}; retrying"
                        );
                        tokio::time::sleep(retry_delay(attempt)).await;
                        last_error = Some(anyhow!("{context} failed for {url}: {err}"));
                        continue;
                    }
                    return Err(err)
                        .with_context(|| format!("{context} failed for {url}"));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("{context} failed for {url}")))
    }
}

fn normalize_tag(tag: &str) -> String {
    if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    }
}

fn should_retry_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::BAD_GATEWAY
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        || status == reqwest::StatusCode::GATEWAY_TIMEOUT
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis((attempt as u64) * 500)
}

fn build_http_error(
    response: reqwest::Response,
    url: &str,
    context: &str,
) -> anyhow::Error {
    let status = response.status();
    let headers = response.headers();

    if matches!(
        status,
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS
    ) && headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        == Some("0")
    {
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown");
        return anyhow!(
            "{context} hit the GitHub rate limit for {url} (status {status}). Reset epoch: {reset}"
        );
    }

    anyhow!("{context} returned {status} for {url}")
}
