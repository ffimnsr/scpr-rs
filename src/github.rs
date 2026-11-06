use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{AUTHORIZATION, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error as StdError;
use std::fs;
use std::path::{Path, PathBuf};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, info, warn};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: usize = 3;
const MAX_DOWNLOAD_SIZE_BYTES: u64 = 500 * 1024 * 1024;
const TOKEN_EXPIRY_WARNING_DAYS: i64 = 14;
const MAX_CONCURRENT_GITHUB_REQUESTS: usize = 4;
const CACHE_PRUNE_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const TOKEN_ENV_VARS: [&str; 2] = ["GITHUB_TOKEN", "GH_TOKEN"];
const TOKEN_HINT: &str = "Set GITHUB_TOKEN or GH_TOKEN to raise the GitHub API limit from 60 to 5,000 requests per hour, for example:\n  export GITHUB_TOKEN=\"$(gh auth token)\"";

/// GitHub release metadata returned by the API or synthesized from the public
/// release page when the API quota is exhausted.
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
    /// `false` when the release was resolved from the public releases page
    /// because the API was unavailable; asset names are then inferred from the
    /// plugin instead of the API response.
    #[serde(default = "default_true")]
    pub assets_known: bool,
}

impl Release {
    fn without_assets(tag_name: String) -> Self {
        Self {
            tag_name,
            assets: Vec::new(),
            assets_known: false,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RepoMetadata {
    pub default_branch: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitStatus {
    pub resources: RateLimitResources,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitResources {
    pub core: RateLimitCore,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitCore {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
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

#[derive(Debug, Deserialize)]
struct CommitRefResponse {
    sha: String,
}

/// A single asset attached to a GitHub release.
#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

/// A cached response body plus the validators needed to revalidate it.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedResponse {
    url: String,
    etag: String,
    fetched_at_unix: u64,
    body: String,
}

/// Thin wrapper around [`reqwest::Client`] for GitHub API calls.
#[derive(Clone)]
pub struct GithubClient {
    client: reqwest::Client,
    token_present: bool,
    token_source: Option<&'static str>,
    warned_invalid_token: Arc<AtomicBool>,
    warned_expiring_token: Arc<AtomicBool>,
    warned_rate_limit: Arc<AtomicBool>,
    request_semaphore: Arc<tokio::sync::Semaphore>,
    rate_limit_reset_unix: Arc<tokio::sync::Mutex<Option<u64>>>,
    cache_dir: Option<PathBuf>,
    cache_ttl: Duration,
    cache_pruned: Arc<AtomicBool>,
}

impl GithubClient {
    /// Build a new client that identifies itself as `scpr/<version>`.
    ///
    /// If `GITHUB_TOKEN` or `GH_TOKEN` is set, it is sent as a `Bearer` token
    /// on every request, raising the API rate limit from 60 to 5 000 requests
    /// per hour and enabling access to private repositories.
    ///
    /// `cache_dir` enables on-disk caching of release metadata and `cache_ttl`
    /// controls how long a cached body is reused without revalidation.
    pub fn new(
        version: &str,
        cache_dir: Option<PathBuf>,
        cache_ttl: Duration,
    ) -> Result<Self> {
        let mut default_headers = HeaderMap::new();
        let token = resolve_github_token();
        let token_present = token.is_some();
        let token_source = token.as_ref().map(|(source, _)| *source);
        if let Some((source, token)) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .with_context(|| format!("{source} contains invalid characters"))?;
            value.set_sensitive(true);
            default_headers.insert(AUTHORIZATION, value);
            info!("{source} detected — using authenticated GitHub API requests");
        } else {
            debug!(
                "No GITHUB_TOKEN or GH_TOKEN set — GitHub API requests are unauthenticated and limited to 60 per hour"
            );
        }
        let client = reqwest::Client::builder()
            .user_agent(format!("scpr/{version}"))
            .timeout(REQUEST_TIMEOUT)
            .default_headers(default_headers)
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            client,
            token_present,
            token_source,
            warned_invalid_token: Arc::new(AtomicBool::new(false)),
            warned_expiring_token: Arc::new(AtomicBool::new(false)),
            warned_rate_limit: Arc::new(AtomicBool::new(false)),
            request_semaphore: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_GITHUB_REQUESTS,
            )),
            rate_limit_reset_unix: Arc::new(tokio::sync::Mutex::new(None)),
            cache_dir,
            cache_ttl,
            cache_pruned: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Fetch the latest release metadata for `owner/repo`.
    ///
    /// Cached metadata is reused while it is younger than the configured TTL.
    pub async fn get_latest_release(&self, owner: &str, repo: &str) -> Result<Release> {
        self.get_latest_release_inner(owner, repo, true).await
    }

    /// Fetch the latest release metadata, bypassing the TTL cache.
    ///
    /// A conditional request is still made when a cached ETag exists, so an
    /// unchanged release costs no API quota.
    pub async fn get_latest_release_fresh(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Release> {
        self.get_latest_release_inner(owner, repo, false).await
    }

    async fn get_latest_release_inner(
        &self,
        owner: &str,
        repo: &str,
        allow_cached: bool,
    ) -> Result<Release> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
        debug!("Fetching latest release: {url}");

        match self
            .get_release_cached(&url, "GitHub API request", allow_cached)
            .await
        {
            Ok(release) => {
                debug!("Latest release tag: {}", release.tag_name);
                Ok(release)
            }
            Err(err) if is_rate_limit_error(&err) => {
                warn!(
                    "GitHub API rate limit is exhausted; resolving the latest release for {owner}/{repo} from the public releases page"
                );
                let tag_name = self
                    .resolve_latest_tag_from_web(owner, repo)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to resolve the latest release for {owner}/{repo} without the GitHub API after the rate limit was hit: {err}"
                        )
                    })?;
                debug!("Resolved latest release tag without the API: {tag_name}");
                Ok(Release::without_assets(tag_name))
            }
            Err(err) => Err(err),
        }
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

    pub async fn resolve_commit_sha(
        &self,
        owner: &str,
        repo: &str,
        r#ref: &str,
    ) -> Result<String> {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/commits/{ref}",
            r#ref = r#ref
        );
        debug!("Resolving commit SHA: {url}");
        let response = self.get_with_retries(&url, "GitHub API request").await?;
        let commit: CommitRefResponse = response
            .json()
            .await
            .context("Failed to parse GitHub commit response")?;
        Ok(commit.sha)
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
        let candidates = release_tag_lookup_candidates(tag);

        for (index, candidate) in candidates.iter().enumerate() {
            let url = format!(
                "https://api.github.com/repos/{owner}/{repo}/releases/tags/{candidate}"
            );
            debug!("Fetching release by tag: {url}");

            match self
                .get_release_cached(&url, "GitHub API request", true)
                .await
            {
                Ok(release) => {
                    debug!("Resolved release tag: {}", release.tag_name);
                    return Ok(release);
                }
                Err(err) if is_rate_limit_error(&err) => {
                    warn!(
                        "GitHub API rate limit is exhausted; resolving release '{tag}' for {owner}/{repo} from the public releases page"
                    );
                    for candidate in &candidates {
                        match self.release_tag_exists_on_web(owner, repo, candidate).await
                        {
                            Ok(true) => {
                                debug!(
                                    "Resolved release tag without the API: {candidate}"
                                );
                                return Ok(Release::without_assets(candidate.clone()));
                            }
                            Ok(false) => {}
                            Err(check_err) => debug!(
                                "Failed to check {owner}/{repo} release tag '{candidate}' without the GitHub API: {check_err}"
                            ),
                        }
                    }
                    return Err(err).with_context(|| {
                        format!(
                            "No published release tag for {owner}/{repo} matched {tag} on the public releases page"
                        )
                    });
                }
                Err(err)
                    if index + 1 < candidates.len()
                        && is_http_status_error(&err, reqwest::StatusCode::NOT_FOUND) =>
                {
                    debug!(
                        "Release tag '{candidate}' was not found for {owner}/{repo}; trying alternate tag form"
                    );
                }
                Err(err) => return Err(err),
            }
        }

        unreachable!("release_tag_lookup_candidates always yields at least one tag")
    }

    pub async fn list_release_tags(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<String>> {
        let url =
            format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page=100");
        debug!("Listing release tags: {url}");

        let response = self.get_with_retries(&url, "GitHub API request").await?;
        let releases: Vec<Release> = response
            .json()
            .await
            .context("Failed to parse GitHub releases response")?;
        Ok(releases
            .into_iter()
            .map(|release| release.tag_name)
            .collect())
    }

    pub async fn get_rate_limit_status(&self) -> Result<RateLimitCore> {
        let response = self
            .get_with_retries("https://api.github.com/rate_limit", "GitHub API request")
            .await?;
        let status: RateLimitStatus = response
            .json()
            .await
            .context("Failed to parse GitHub rate limit response")?;
        Ok(status.resources.core)
    }

    /// Download `url` while displaying a progress bar.
    ///
    /// `expected_size` is used to size the progress bar; pass `0` if unknown.
    pub async fn download_asset(&self, url: &str, expected_size: u64) -> Result<Vec<u8>> {
        debug!("Downloading asset: {url}");
        let mut success_response = None;

        for attempt in 1..=MAX_RETRIES {
            let permit = self
                .request_semaphore
                .clone()
                .acquire_owned()
                .await
                .context("GitHub request semaphore closed")?;
            match self.client.get(url).send().await {
                Ok(response) if response.status().is_success() => {
                    self.update_rate_limit_state(response.headers()).await;
                    success_response = Some((response, permit));
                    break;
                }
                Ok(response) => {
                    let status = response.status();
                    self.update_rate_limit_state(response.headers()).await;
                    let error = build_http_error(response, url, "asset download request");
                    if should_retry_status(status) && attempt < MAX_RETRIES {
                        warn!(
                            "asset download request failed with {status} on attempt {attempt}/{MAX_RETRIES}; retrying"
                        );
                    } else {
                        return Err(error);
                    }
                }
                Err(err) => {
                    let retryable =
                        (err.is_timeout() || err.is_connect()) && attempt < MAX_RETRIES;
                    if retryable {
                        warn!(
                            "asset download request failed on attempt {attempt}/{MAX_RETRIES}: {err}; retrying"
                        );
                    } else {
                        return Err(err).with_context(|| {
                            format!("asset download request failed for {url}")
                        });
                    }
                }
            }
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        let (response, _permit) = success_response.unwrap_or_else(|| {
            unreachable!(
                "download_asset either returns an error or breaks with a response"
            )
        });

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
        self.get_with_retries_conditional(url, context, None)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "{context} returned 304 Not Modified for {url} without a cached response"
                )
            })
    }

    /// Perform a GET request, optionally revalidating a cached body with
    /// `If-None-Match`. Returns `Ok(None)` for `304 Not Modified`.
    async fn get_with_retries_conditional(
        &self,
        url: &str,
        context: &str,
        etag: Option<&str>,
    ) -> Result<Option<reqwest::Response>> {
        let mut last_error = None;

        for attempt in 1..=MAX_RETRIES {
            self.ensure_rate_limit_window(url).await?;
            let _permit = self
                .request_semaphore
                .acquire()
                .await
                .context("GitHub request semaphore closed")?;
            let mut request = self.client.get(url);
            if let Some(etag) = etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            match request.send().await {
                Ok(response)
                    if response.status() == reqwest::StatusCode::NOT_MODIFIED =>
                {
                    self.update_rate_limit_state(response.headers()).await;
                    self.warn_on_token_expiry(response.headers());
                    debug!("Revalidated cached GitHub response (304) for {url}");
                    return Ok(None);
                }
                Ok(response) if response.status().is_success() => {
                    self.update_rate_limit_state(response.headers()).await;
                    self.warn_on_token_expiry(response.headers());
                    return Ok(Some(response));
                }
                Ok(response) => {
                    let status = response.status();
                    let should_retry =
                        should_retry_status(status) && attempt < MAX_RETRIES;
                    self.update_rate_limit_state(response.headers()).await;
                    self.warn_on_invalid_token(status);
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

    /// Return release metadata from the cache when allowed and fresh, and
    /// otherwise revalidate or refetch it.
    ///
    /// When the API quota is exhausted, a stale cached body is preferred over
    /// failing the command.
    async fn get_release_cached(
        &self,
        url: &str,
        context: &str,
        allow_fresh_cache: bool,
    ) -> Result<Release> {
        let cached = self.read_cached_response(url);
        if allow_fresh_cache
            && let Some(cached) = &cached
            && self.cache_is_fresh(cached)
        {
            match serde_json::from_str::<Release>(&cached.body) {
                Ok(release) => {
                    debug!("Using cached GitHub release metadata for {url}");
                    return Ok(release);
                }
                Err(err) => {
                    debug!("Ignoring unreadable cached response for {url}: {err}");
                }
            }
        }

        let etag = cached.as_ref().map(|entry| entry.etag.as_str());
        match self.get_with_retries_conditional(url, context, etag).await {
            Ok(Some(response)) => {
                let etag = response
                    .headers()
                    .get(ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = response
                    .text()
                    .await
                    .context("Failed to read GitHub release response")?;
                let release: Release = serde_json::from_str(&body)
                    .context("Failed to parse GitHub release response")?;
                if let Some(etag) = etag {
                    self.write_cached_response(&CachedResponse {
                        url: url.to_string(),
                        etag,
                        fetched_at_unix: unix_timestamp_now(),
                        body,
                    });
                }
                Ok(release)
            }
            Ok(None) => {
                let cached = cached.ok_or_else(|| {
                    anyhow!(
                        "GitHub returned 304 Not Modified for {url} but no cached response was available"
                    )
                })?;
                self.touch_cached_response(&cached);
                serde_json::from_str(&cached.body)
                    .context("Failed to parse cached GitHub release response")
            }
            Err(err) if is_rate_limit_error(&err) => {
                if let Some(cached) = cached {
                    warn!(
                        "GitHub API rate limit is exhausted; using cached release metadata for {url}"
                    );
                    serde_json::from_str(&cached.body)
                        .context("Failed to parse cached GitHub release response")
                } else {
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    fn cache_file_path(&self, url: &str) -> Option<PathBuf> {
        let dir = self.cache_dir.as_ref()?;
        let mut filename = String::with_capacity(69);
        for byte in Sha256::digest(url.as_bytes()) {
            filename.push_str(&format!("{byte:02x}"));
        }
        filename.push_str(".json");
        Some(dir.join(filename))
    }

    fn read_cached_response(&self, url: &str) -> Option<CachedResponse> {
        let path = self.cache_file_path(url)?;
        let data = fs::read_to_string(path).ok()?;
        let cached: CachedResponse = serde_json::from_str(&data).ok()?;
        (cached.url == url).then_some(cached)
    }

    fn cache_is_fresh(&self, cached: &CachedResponse) -> bool {
        !self.cache_ttl.is_zero()
            && unix_timestamp_now().saturating_sub(cached.fetched_at_unix)
                < self.cache_ttl.as_secs()
    }

    fn write_cached_response(&self, cached: &CachedResponse) {
        let Some(path) = self.cache_file_path(&cached.url) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if let Err(err) = create_cache_dir(parent) {
            debug!(
                "Failed to create GitHub cache directory {}: {err}",
                parent.display()
            );
            return;
        }

        let data = match serde_json::to_vec(cached) {
            Ok(data) => data,
            Err(err) => {
                debug!("Failed to serialize GitHub cache entry: {err}");
                return;
            }
        };
        let temp_path = path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        if let Err(err) =
            fs::write(&temp_path, data).and_then(|()| fs::rename(&temp_path, &path))
        {
            let _ = fs::remove_file(&temp_path);
            debug!(
                "Failed to write GitHub cache entry {}: {err}",
                path.display()
            );
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        self.prune_cache_if_needed();
    }

    /// Remove cache entries that have not been touched for a week. Runs at most
    /// once per process, after a successful write.
    fn prune_cache_if_needed(&self) {
        if self.cache_pruned.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(dir) = self.cache_dir.as_ref() else {
            return;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        let now = SystemTime::now();
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            let age = now.duration_since(modified).unwrap_or_default().as_secs();
            if age > CACHE_PRUNE_AGE_SECS {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn touch_cached_response(&self, cached: &CachedResponse) {
        self.write_cached_response(&CachedResponse {
            fetched_at_unix: unix_timestamp_now(),
            ..cached.clone()
        });
    }

    /// Resolve the latest release tag from the public releases page. This does
    /// not consume GitHub REST API quota and only works for public repos.
    async fn resolve_latest_tag_from_web(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<String> {
        let _permit = self
            .request_semaphore
            .acquire()
            .await
            .context("GitHub request semaphore closed")?;
        let url = format!("https://github.com/{owner}/{repo}/releases/latest");
        debug!("Resolving latest release without the GitHub API: {url}");
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch {url}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("{url} returned {status}"));
        }

        let final_url = response.url().clone();
        parse_release_tag_from_url(final_url.path()).ok_or_else(|| {
            anyhow!(
                "GitHub did not redirect {url} to a release tag (final URL: {final_url})"
            )
        })
    }

    /// Check whether a tag has a public release page, without the REST API.
    async fn release_tag_exists_on_web(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<bool> {
        let _permit = self
            .request_semaphore
            .acquire()
            .await
            .context("GitHub request semaphore closed")?;
        let mut url = reqwest::Url::parse(&format!(
            "https://github.com/{owner}/{repo}/releases/tag/"
        ))
        .context("Failed to build GitHub release page URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("Failed to build GitHub release page URL"))?
            .push(tag);

        let response = self
            .client
            .head(url.clone())
            .send()
            .await
            .with_context(|| format!("Failed to check {url}"))?;
        Ok(response.status().is_success())
    }

    /// Fail fast when the quota is already known to be exhausted instead of
    /// sleeping until the reset time. The `/rate_limit` endpoint is exempt
    /// because GitHub does not count it against the quota, and `scpr doctor`
    /// uses it to report the current state.
    async fn ensure_rate_limit_window(&self, url: &str) -> Result<()> {
        if url.ends_with("/rate_limit") {
            return Ok(());
        }

        let reset_epoch = { *self.rate_limit_reset_unix.lock().await };
        let Some(reset_epoch) = reset_epoch else {
            return Ok(());
        };
        if unix_timestamp_now() >= reset_epoch {
            let mut guard = self.rate_limit_reset_unix.lock().await;
            *guard = None;
            return Ok(());
        }

        Err(rate_limit_error(reset_epoch))
    }

    async fn update_rate_limit_state(&self, headers: &HeaderMap) {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let reset_epoch = headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());

        let Some(remaining) = remaining else {
            return;
        };
        if remaining != 0 {
            return;
        }
        let Some(reset_epoch) = reset_epoch else {
            return;
        };

        let mut guard = self.rate_limit_reset_unix.lock().await;
        *guard = Some(reset_epoch);
        drop(guard);

        if !self.warned_rate_limit.swap(true, Ordering::Relaxed) {
            warn!(
                "GitHub API rate limit is exhausted. Quota resets at {} (in {}). {TOKEN_HINT}",
                format_reset_epoch(reset_epoch),
                format_wait_duration(reset_epoch),
            );
        }
    }

    fn warn_on_invalid_token(&self, status: reqwest::StatusCode) {
        if !self.token_present || status != reqwest::StatusCode::UNAUTHORIZED {
            return;
        }

        if !self.warned_invalid_token.swap(true, Ordering::Relaxed) {
            let source = self.token_source.unwrap_or("GitHub token");
            warn!(
                "{source} was rejected by the GitHub API (401 Unauthorized). Check whether it is invalid, expired, or missing required scopes."
            );
        }
    }

    fn warn_on_token_expiry(&self, headers: &HeaderMap) {
        if !self.token_present || self.warned_expiring_token.load(Ordering::Relaxed) {
            return;
        }

        let Some(raw_expiry) = headers
            .get("github-authentication-token-expiration")
            .and_then(|value| value.to_str().ok())
        else {
            return;
        };

        let parsed =
            chrono::NaiveDateTime::parse_from_str(raw_expiry, "%Y-%m-%d %H:%M:%S %Z")
                .or_else(|_| {
                    chrono::DateTime::parse_from_rfc3339(raw_expiry)
                        .map(|value| value.naive_utc())
                });

        let Ok(expiry) = parsed else {
            debug!(
                "Could not parse GitHub token expiration header: {}",
                raw_expiry
            );
            return;
        };

        let now = chrono::Utc::now().naive_utc();
        let remaining = expiry - now;
        if remaining.num_days() <= TOKEN_EXPIRY_WARNING_DAYS
            && !self.warned_expiring_token.swap(true, Ordering::Relaxed)
        {
            warn!(
                "{} expires on {} UTC (about {} day(s) remaining). Rotate it soon to avoid interrupted installs and update checks.",
                self.token_source.unwrap_or("GitHub token"),
                expiry.format("%Y-%m-%d %H:%M:%S"),
                remaining.num_days().max(0)
            );
        }
    }
}

fn resolve_github_token() -> Option<(&'static str, String)> {
    resolve_github_token_from(|name| std::env::var(name).ok())
}

fn resolve_github_token_from(
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Option<(&'static str, String)> {
    TOKEN_ENV_VARS.iter().find_map(|name| {
        let value = lookup(name)?;
        let token = value.trim();
        (!token.is_empty()).then(|| (*name, token.to_string()))
    })
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn format_reset_epoch(epoch_secs: u64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("Unix epoch {epoch_secs}"))
}

fn format_wait_duration(epoch_secs: u64) -> String {
    let seconds = epoch_secs.saturating_sub(unix_timestamp_now());
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn rate_limit_error(reset_epoch: u64) -> anyhow::Error {
    anyhow::Error::new(RateLimitError {
        message: format!(
            "GitHub API rate limit is exhausted. Quota resets at {} (in {}).\n{TOKEN_HINT}",
            format_reset_epoch(reset_epoch),
            format_wait_duration(reset_epoch),
        ),
    })
}

fn default_true() -> bool {
    true
}

fn create_cache_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Extract the tag from a GitHub release page path such as
/// `/owner/repo/releases/tag/v1.2.3`.
fn parse_release_tag_from_url(path: &str) -> Option<String> {
    const MARKER: &str = "/releases/tag/";
    let index = path.find(MARKER)?;
    let tag = &path[index + MARKER.len()..];
    (!tag.is_empty()).then(|| tag.to_string())
}

fn release_tag_lookup_candidates(tag: &str) -> Vec<String> {
    let alternate = if let Some(stripped) = tag.strip_prefix('v') {
        stripped.to_string()
    } else {
        format!("v{tag}")
    };

    if alternate == tag {
        vec![tag.to_string()]
    } else {
        vec![tag.to_string(), alternate]
    }
}

fn is_http_status_error(error: &anyhow::Error, status: reqwest::StatusCode) -> bool {
    error
        .downcast_ref::<HttpStatusError>()
        .map(|inner| inner.status == status)
        .unwrap_or(false)
}

fn is_rate_limit_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RateLimitError>().is_some()
}

#[derive(Debug)]
struct RateLimitError {
    message: String,
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for RateLimitError {}

#[derive(Debug)]
struct HttpStatusError {
    status: reqwest::StatusCode,
    message: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for HttpStatusError {}

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
            .and_then(|value| value.parse::<u64>().ok());
        return match reset {
            Some(reset_epoch) => rate_limit_error(reset_epoch).context(format!(
                "{context} hit the GitHub rate limit for {url} (status {status})"
            )),
            None => anyhow::Error::new(RateLimitError {
                message: format!(
                    "{context} hit the GitHub rate limit for {url} (status {status}).\n{TOKEN_HINT}"
                ),
            }),
        };
    }

    anyhow::Error::new(HttpStatusError {
        status,
        message: format!("{context} returned {status} for {url}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CachedResponse, GithubClient, format_reset_epoch, is_rate_limit_error,
        parse_release_tag_from_url, rate_limit_error, release_tag_lookup_candidates,
        resolve_github_token_from, unix_timestamp_now,
    };
    use std::time::Duration;

    #[test]
    fn release_tag_lookup_candidates_try_exact_tag_before_v_prefix() {
        assert_eq!(
            release_tag_lookup_candidates("0.7.13"),
            vec!["0.7.13".to_string(), "v0.7.13".to_string()]
        );
    }

    #[test]
    fn release_tag_lookup_candidates_try_exact_v_tag_before_plain_version() {
        assert_eq!(
            release_tag_lookup_candidates("v0.7.13"),
            vec!["v0.7.13".to_string(), "0.7.13".to_string()]
        );
    }

    #[test]
    fn resolve_github_token_prefers_github_token_and_trims_whitespace() {
        let token = resolve_github_token_from(|name| match name {
            "GITHUB_TOKEN" => Some("  primary-token  ".to_string()),
            "GH_TOKEN" => Some("fallback-token".to_string()),
            _ => None,
        });
        assert_eq!(token, Some(("GITHUB_TOKEN", "primary-token".to_string())));
    }

    #[test]
    fn resolve_github_token_falls_back_to_gh_token_and_skips_empty_values() {
        let token = resolve_github_token_from(|name| match name {
            "GITHUB_TOKEN" => Some("   ".to_string()),
            "GH_TOKEN" => Some("fallback-token".to_string()),
            _ => None,
        });
        assert_eq!(token, Some(("GH_TOKEN", "fallback-token".to_string())));
    }

    #[test]
    fn resolve_github_token_returns_none_when_unset() {
        assert_eq!(resolve_github_token_from(|_| None), None);
    }

    #[test]
    fn format_reset_epoch_renders_utc_timestamp() {
        assert_eq!(format_reset_epoch(0), "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn parse_release_tag_from_url_extracts_tag() {
        assert_eq!(
            parse_release_tag_from_url("/acme/tool/releases/tag/v1.2.3"),
            Some("v1.2.3".to_string())
        );
        assert_eq!(parse_release_tag_from_url("/acme/tool/releases"), None);
        assert_eq!(parse_release_tag_from_url("/acme/tool/releases/tag/"), None);
        assert_eq!(
            parse_release_tag_from_url("/tag/tool/releases/tag/v2.0.0"),
            Some("v2.0.0".to_string())
        );
    }

    #[test]
    fn rate_limit_errors_remain_typed_after_context() {
        let error =
            rate_limit_error(unix_timestamp_now() + 60).context("GitHub API request");
        assert!(is_rate_limit_error(&error));
    }

    #[test]
    fn cache_roundtrip_preserves_body_and_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let client = GithubClient::new(
            "test",
            Some(dir.path().to_path_buf()),
            Duration::from_secs(300),
        )
        .unwrap();
        let url = "https://api.github.com/repos/acme/tool/releases/latest";
        let cached = CachedResponse {
            url: url.to_string(),
            etag: "\"abc123\"".to_string(),
            fetched_at_unix: unix_timestamp_now(),
            body: "{\"tag_name\":\"v1.2.3\",\"assets\":[]}".to_string(),
        };

        client.write_cached_response(&cached);
        let read = client.read_cached_response(url).expect("cache entry");

        assert_eq!(read.body, cached.body);
        assert_eq!(read.etag, cached.etag);
        assert!(client.cache_is_fresh(&read));
    }

    #[test]
    fn older_cache_entries_are_not_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let client = GithubClient::new(
            "test",
            Some(dir.path().to_path_buf()),
            Duration::from_secs(300),
        )
        .unwrap();
        let cached = CachedResponse {
            url: "https://api.github.com/repos/acme/tool/releases/latest".to_string(),
            etag: "\"abc123\"".to_string(),
            fetched_at_unix: unix_timestamp_now().saturating_sub(3600),
            body: "{}".to_string(),
        };

        assert!(!client.cache_is_fresh(&cached));
    }

    #[test]
    fn zero_ttl_disables_fresh_cache_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let client =
            GithubClient::new("test", Some(dir.path().to_path_buf()), Duration::ZERO)
                .unwrap();
        let cached = CachedResponse {
            url: "https://api.github.com/repos/acme/tool/releases/latest".to_string(),
            etag: "\"abc123\"".to_string(),
            fetched_at_unix: unix_timestamp_now(),
            body: "{}".to_string(),
        };

        assert!(!client.cache_is_fresh(&cached));
    }
}
