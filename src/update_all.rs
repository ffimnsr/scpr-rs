use crate::{cli_support, github, installer, plugin, settings};
use anyhow::Result;
use futures_util::stream::{self, StreamExt};

/// Maximum number of concurrent package updates during `update --all`.
const UPDATE_ALL_CONCURRENCY: usize = 4;

pub(crate) struct UpdateAllOptions<'a> {
    pub(crate) dirs: &'a [String],
    pub(crate) force_refresh: bool,
    pub(crate) dry_run: bool,
    pub(crate) build: bool,
}

/// Update every outdated, unpinned installed package.
///
/// Packages whose update check fails - for example because their repository is
/// gone, renamed, or has no published release - are skipped instead of aborting
/// the run, so the remaining packages still update. Failures are reported once
/// the updates complete.
pub(crate) async fn run(
    installer: &installer::Installer,
    client: &github::GithubClient,
    settings: &settings::AppSettings,
    options: UpdateAllOptions<'_>,
) -> Result<()> {
    let UpdateAllOptions {
        dirs,
        force_refresh,
        dry_run,
        build,
    } = options;

    let cli_support::OutdatedPackagesReport {
        outdated,
        failures: check_failures,
    } = cli_support::collect_outdated_packages_report(
        installer, client, dirs, None, true,
    )
    .await?;

    let mut update_failures: Vec<(String, String)> = Vec::new();

    if outdated.is_empty() {
        if check_failures.is_empty() {
            println!("All installed packages are up to date.");
        } else {
            println!("No package updates to apply.");
        }
    } else {
        let installer = installer.clone();
        let client = client.clone();
        let settings = settings.clone();
        let package_count = outdated.len();
        let mut updates = stream::iter(outdated.into_iter().map(|entry| {
            let installer = installer.clone();
            let client = client.clone();
            let settings = settings.clone();
            async move {
                let package_name = entry.name.clone();
                let package_dirs = match cli_support::resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    None,
                    &entry.name,
                    force_refresh,
                )
                .await
                {
                    Ok(dirs) => dirs,
                    Err(err) => return Err((package_name, err)),
                };
                let plugin = match plugin::find_plugin(&entry.name, &package_dirs) {
                    Ok(plugin) => plugin,
                    Err(err) => return Err((package_name, err)),
                };
                match installer
                    .install(
                        &plugin,
                        &client,
                        installer::InstallOptions {
                            tag: Some(&entry.latest_version),
                            target_override: None,
                            dry_run,
                            force: false,
                            build,
                        },
                    )
                    .await
                {
                    Ok(()) => Ok(entry.name),
                    Err(err) => Err((entry.name, err)),
                }
            }
        }))
        .buffer_unordered(UPDATE_ALL_CONCURRENCY);

        let mut succeeded = Vec::new();
        let mut completed = 0usize;
        while let Some(result) = updates.next().await {
            completed += 1;
            match result {
                Ok(name) => {
                    println!(
                        "Progress: {completed}/{package_count} complete ({name}: updated)"
                    );
                    succeeded.push(name);
                }
                Err((name, err)) => {
                    eprintln!(
                        "Progress: {completed}/{package_count} complete ({name}: failed)"
                    );
                    update_failures.push((name, err.to_string()));
                }
            }
        }

        if !update_failures.is_empty() {
            eprintln!(
                "One or more packages failed during `update --all`. Successful updates were kept."
            );
            for (name, err) in &update_failures {
                eprintln!("  {name}: {err}");
            }
        }

        println!(
            "Update summary: {} succeeded, {} failed, {} total.",
            succeeded.len(),
            update_failures.len(),
            package_count
        );
        if !succeeded.is_empty() {
            println!("Succeeded: {}", succeeded.join(", "));
        }
        if !update_failures.is_empty() {
            println!(
                "Failed: {}",
                update_failures
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    if let Some(message) = update_all_failure(&check_failures, &update_failures) {
        anyhow::bail!("{message}");
    }

    Ok(())
}

/// Build the error reported when `update --all` finished with failures.
///
/// Returns `None` when every package was updated successfully. Check failures
/// are listed before update failures.
fn update_all_failure(
    check_failures: &[(String, String)],
    update_failures: &[(String, String)],
) -> Option<String> {
    let summary = match (check_failures.is_empty(), update_failures.is_empty()) {
        (true, true) => return None,
        (false, false) => "`update --all` finished with check and update failures:",
        (false, true) => "Failed to check updates for one or more packages:",
        (true, false) => "One or more package updates failed during `update --all`:",
    };

    let details = check_failures
        .iter()
        .chain(update_failures.iter())
        .map(|(name, err)| format!("{name}: {err}"))
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!("{summary}\n{details}"))
}

#[cfg(test)]
mod tests {
    use super::update_all_failure;

    fn failures(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, err)| (name.to_string(), err.to_string()))
            .collect()
    }

    #[test]
    fn test_update_all_failure_none_when_nothing_failed() {
        assert_eq!(update_all_failure(&[], &[]), None);
    }

    #[test]
    fn test_update_all_failure_reports_check_failures_only() {
        let check = failures(&[("sqlx", "GitHub API request returned 404 Not Found")]);
        let message = update_all_failure(&check, &[]).expect("expected failure message");
        assert_eq!(
            message,
            "Failed to check updates for one or more packages:\n\
             sqlx: GitHub API request returned 404 Not Found"
        );
    }

    #[test]
    fn test_update_all_failure_reports_update_failures_only() {
        let update = failures(&[("ripgrep", "checksum mismatch")]);
        let message = update_all_failure(&[], &update).expect("expected failure message");
        assert_eq!(
            message,
            "One or more package updates failed during `update --all`:\n\
             ripgrep: checksum mismatch"
        );
    }

    #[test]
    fn test_update_all_failure_lists_check_failures_before_update_failures() {
        let check = failures(&[("sqlx", "GitHub API request returned 404 Not Found")]);
        let update = failures(&[("ripgrep", "checksum mismatch")]);
        let message =
            update_all_failure(&check, &update).expect("expected failure message");
        let header = "`update --all` finished with check and update failures:";
        assert!(message.starts_with(header));
        let check_line = message.find("sqlx: ").expect("check failure missing");
        let update_line = message.find("ripgrep: ").expect("update failure missing");
        assert!(check_line < update_line);
    }
}
