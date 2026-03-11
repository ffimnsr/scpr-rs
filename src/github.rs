use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use tracing::debug;

/// A GitHub release returned by the releases API.
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

/// A single asset attached to a GitHub release.
#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

/// Thin wrapper around [`reqwest::Client`] for GitHub API calls.
pub struct GithubClient {
    client: reqwest::Client,
}

impl GithubClient {
    /// Build a new client that identifies itself as `scarper/<version>`.
    pub fn new(version: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(format!("scarper/{version}"))
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self { client })
    }

    /// Fetch the latest release metadata for `owner/repo`.
    pub async fn get_latest_release(&self, owner: &str, repo: &str) -> Result<Release> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
        debug!("Fetching latest release: {url}");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send request to GitHub API")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "GitHub API returned {} for {}",
                response.status(),
                url
            ));
        }

        let release: Release = response
            .json()
            .await
            .context("Failed to parse GitHub release response")?;

        debug!("Latest release tag: {}", release.tag_name);
        Ok(release)
    }

    /// Download `url` while displaying a progress bar.
    ///
    /// `expected_size` is used to size the progress bar; pass `0` if unknown.
    pub async fn download_asset(&self, url: &str, expected_size: u64) -> Result<Vec<u8>> {
        debug!("Downloading asset: {url}");

        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to send download request")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Download failed with status {} for {}",
                response.status(),
                url
            ));
        }

        let total = response
            .content_length()
            .unwrap_or(expected_size);

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

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read response chunk")?;
            pb.inc(chunk.len() as u64);
            data.extend_from_slice(&chunk);
        }

        pb.finish_with_message("Download complete");
        Ok(data)
    }
}
