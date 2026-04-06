use anyhow::Result;
use clap::{Arg, ArgAction, Command};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use std::path::Path;
use std::process;
use tracing::warn;
use tracing_subscriber::EnvFilter;

mod github;
mod installer;
mod plugin;
mod remote_index;
mod settings;

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");
const UPDATE_ALL_CONCURRENCY: usize = 4;

fn add_plugins_dir_arg(dirs: &mut Vec<String>, extra: Option<&String>) {
    if let Some(extra) = extra {
        dirs.insert(0, extra.clone());
    }
}

async fn resolved_plugin_dirs(
    settings: &settings::AppSettings,
    client: &github::GithubClient,
    extra: Option<&String>,
    force_refresh: bool,
) -> Result<Vec<String>> {
    let mut dirs = settings.default_plugin_dirs();
    add_plugins_dir_arg(&mut dirs, extra);

    let remote_manager = remote_index::RemoteIndexManager::new()?;
    let remote_dirs = remote_manager
        .sync_all(client, settings.index_ttl_secs(), force_refresh)
        .await?;
    if !remote_dirs.is_empty() {
        let insert_at = usize::from(extra.is_some()) + 1;
        for (offset, dir) in remote_dirs.into_iter().enumerate() {
            dirs.insert(insert_at + offset, dir.to_string_lossy().to_string());
        }
    }

    Ok(dirs)
}

async fn resolved_plugin_dirs_for_query(
    settings: &settings::AppSettings,
    client: &github::GithubClient,
    extra: Option<&String>,
    query: &str,
    force_refresh: bool,
) -> Result<Vec<String>> {
    let mut dirs = resolved_plugin_dirs(settings, client, extra, force_refresh).await?;
    let manager = remote_index::RemoteIndexManager::new()?;
    apply_preferred_remote_pin_to_dirs(&manager, &mut dirs, query, extra.is_some())?;
    Ok(dirs)
}

#[derive(Debug, Serialize)]
struct OutdatedPackage {
    name: String,
    current_version: String,
    latest_version: String,
}

#[derive(Debug)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug)]
struct PackageRequest {
    name: String,
    tag: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstalledPackageStatus {
    name: String,
    version: String,
    binary: String,
    pinned: bool,
    installed_at_unix: Option<u64>,
    latest_version: Option<String>,
    outdated: bool,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let plugins_dir_arg = Arg::new("plugins-dir")
        .long("plugins-dir")
        .short('p')
        .help("Additional plugins directory to search")
        .action(clap::ArgAction::Set);

    let dry_run_arg = Arg::new("dry-run")
        .long("dry-run")
        .action(ArgAction::SetTrue)
        .help("Show what would happen without making any changes");

    let json_arg = Arg::new("json")
        .long("json")
        .action(ArgAction::SetTrue)
        .help("Output results as JSON");

    let quiet_arg = Arg::new("quiet")
        .long("quiet")
        .short('q')
        .global(true)
        .action(ArgAction::SetTrue)
        .conflicts_with("verbose")
        .help("Reduce output to warnings and errors");

    let verbose_arg = Arg::new("verbose")
        .long("verbose")
        .short('v')
        .global(true)
        .action(ArgAction::Count)
        .help("Increase output verbosity (-v for debug, -vv for trace)");

    let refresh_arg = Arg::new("refresh")
        .long("refresh")
        .global(true)
        .action(ArgAction::SetTrue)
        .help("Force refresh remote plugin indexes instead of using cached TTL");

    let target_arg = Arg::new("target")
        .long("target")
        .action(ArgAction::Set)
        .help("Override the resolved release target triple");

    let mut cli_app = Command::new("scpr")
        .bin_name("scpr")
        .version(PKG_VERSION)
        .about(PKG_DESCRIPTION)
        .arg_required_else_help(true)
        .propagate_version(true)
        .arg(quiet_arg)
        .arg(verbose_arg)
        .arg(refresh_arg)
        .subcommand(
            Command::new("install")
                .about("Install one or more packages")
                .arg(
                    Arg::new("packages")
                        .required(true)
                        .num_args(1..)
                        .help("Plugin name or alias, optionally as <name>@<tag>"),
                )
                .arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Set)
                        .help("Install a specific release tag (single package only)"),
                )
                .arg(target_arg.clone())
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("uninstall")
                .about("Uninstall a previously installed package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Plugin name or alias"),
                )
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("update")
                .about("Update an installed package, or all packages with --all")
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Plugin name or alias, optionally as <name>@<tag>"),
                )
                .arg(
                    Arg::new("all")
                        .long("all")
                        .action(ArgAction::SetTrue)
                        .help("Update all installed packages (pinned packages are skipped)"),
                )
                .arg(
                    Arg::new("tag")
                        .long("tag")
                        .action(ArgAction::Set)
                        .help("Update to a specific release tag"),
                )
                .arg(target_arg.clone())
                .arg(dry_run_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("plugins")
                .about("Inspect available plugins")
                .subcommand(
                    Command::new("list")
                        .about("List all available plugins")
                        .arg(json_arg.clone())
                        .arg(plugins_dir_arg.clone()),
                )
                .subcommand(
                    Command::new("search")
                        .about("Search available plugins")
                        .arg(
                            Arg::new("query")
                                .help(
                                    "Case-insensitive plugin name, alias, or description filter",
                                )
                                .required(false),
                        )
                        .arg(json_arg.clone())
                        .arg(plugins_dir_arg.clone()),
                )
                .subcommand(
                    Command::new("info")
                        .about("Show details about an available plugin")
                        .arg(
                            Arg::new("package")
                                .required(true)
                                .help("Plugin name or alias"),
                        )
                        .arg(plugins_dir_arg.clone()),
                )
                .subcommand(
                    Command::new("index")
                        .about("Manage remote plugin indexes")
                        .subcommand(
                            Command::new("add")
                                .about("Add a remote plugin index from a GitHub repository")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("list")
                                .about("List configured remote plugin indexes")
                                .arg(json_arg.clone()),
                        )
                        .subcommand(
                            Command::new("enable")
                                .about("Enable a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("disable")
                                .about("Disable a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("remove")
                                .about("Remove a configured remote plugin index")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("sync")
                                .about("Sync one configured remote plugin index, or all with --all")
                                .arg(
                                    Arg::new("repo")
                                        .required(false)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                )
                                .arg(
                                    Arg::new("all")
                                        .long("all")
                                        .action(ArgAction::SetTrue)
                                        .help("Sync all enabled remote plugin indexes"),
                                ),
                        )
                        .subcommand(
                            Command::new("promote")
                                .about("Move an index earlier in plugin resolution order")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("demote")
                                .about("Move an index later in plugin resolution order")
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("pin")
                                .about("Pin a plugin to prefer a specific remote plugin index")
                                .arg(
                                    Arg::new("plugin")
                                        .required(true)
                                        .help("Plugin name or alias"),
                                )
                                .arg(
                                    Arg::new("repo")
                                        .required(true)
                                        .help("GitHub repo in the form <owner>/<repo>"),
                                ),
                        )
                        .subcommand(
                            Command::new("unpin")
                                .about("Remove a plugin's preferred remote plugin index")
                                .arg(
                                    Arg::new("plugin")
                                        .required(true)
                                        .help("Plugin name"),
                                ),
                        )
                        .subcommand(
                            Command::new("pins")
                                .about("List plugin-specific remote index pins")
                                .arg(json_arg.clone()),
                        ),
                ),
        )
        .subcommand(
            Command::new("list")
                .about("List all installed packages")
                .arg(
                    Arg::new("outdated")
                        .long("outdated")
                        .action(ArgAction::SetTrue)
                        .help("Include latest-version status inline"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("outdated")
                .about("List installed packages with newer releases")
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Installed package name or alias"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(Command::new("doctor").about("Check the local installer setup"))
        .subcommand(
            Command::new("status")
                .about("Show installed packages (alias for list)")
                .arg(
                    Arg::new("outdated")
                        .long("outdated")
                        .action(ArgAction::SetTrue)
                        .help("Include latest-version status inline"),
                )
                .arg(json_arg.clone())
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("verify")
                .about("Alias for audit"),
        )
        .subcommand(
            Command::new("audit")
                .about("Audit installed binaries for missing or modified files")
                .arg(json_arg.clone()),
        )
        .subcommand(
            Command::new("history")
                .about("Show package install, update, remove, and pin history")
                .args_conflicts_with_subcommands(true)
                .arg(
                    Arg::new("package")
                        .required(false)
                        .help("Filter history to a single package"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .action(ArgAction::Set)
                        .value_parser(clap::value_parser!(usize))
                        .help("Show only the most recent N history events"),
                )
                .arg(
                    Arg::new("graph")
                        .long("graph")
                        .action(ArgAction::SetTrue)
                        .help("Group history by package as a simple movement graph"),
                )
                .arg(json_arg.clone())
                .subcommand(
                    Command::new("clear")
                        .about("Clear history for all packages, or one package if provided")
                        .arg(
                            Arg::new("package")
                                .required(false)
                                .help("Clear history only for one package"),
                        ),
                ),
        )
        .subcommand(
            Command::new("export")
                .about("Export installed state and history")
                .arg(
                    Arg::new("path")
                        .required(false)
                        .help("Write to a file instead of stdout"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .action(ArgAction::Set)
                        .default_value("json")
                        .value_parser(["json", "toml"])
                        .help("Export format"),
                ),
        )
        .subcommand(
            Command::new("restore")
                .about("Restore installed state and history from a backup file")
                .arg(
                    Arg::new("path")
                        .required(true)
                        .help("Backup file to restore from"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .action(ArgAction::Set)
                        .value_parser(["json", "toml"])
                        .help("Backup format (defaults to file extension)"),
                ),
        )
        .subcommand(
            Command::new("pin")
                .about("Pin a package so `update --all` skips it")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Installed package name"),
                ),
        )
        .subcommand(
            Command::new("unpin")
                .about("Remove the pin from an installed package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Installed package name"),
                ),
        )
        .subcommand(
            Command::new("completions")
                .about("Print shell completion script to stdout")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .help("Shell to generate completions for (bash, zsh, fish, elvish, powershell)"),
                ),
        );

    let matches = cli_app.clone().get_matches();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if matches.get_flag("quiet") {
            EnvFilter::new("warn")
        } else {
            match matches.get_count("verbose") {
                0 => EnvFilter::new("info"),
                1 => EnvFilter::new("debug"),
                _ => EnvFilter::new("trace"),
            }
        }
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .init();

    let settings = settings::AppSettings::load()?;
    let force_refresh = matches.get_flag("refresh");

    let client = github::GithubClient::new(PKG_VERSION)?;
    let installer = installer::Installer::new()?;

    match matches.subcommand() {
        Some(("install", sub)) => {
            let package_values: Vec<&String> =
                sub.get_many::<String>("packages").unwrap().collect();
            if package_values.len() > 1 && sub.get_one::<String>("tag").is_some() {
                anyhow::bail!("`install --tag` only supports a single package");
            }
            if package_values.len() > 1 && sub.get_one::<String>("target").is_some() {
                anyhow::bail!("`install --target` only supports a single package");
            }
            let dry_run = sub.get_flag("dry-run");
            for package in package_values {
                let request = parse_package_request(
                    package,
                    sub.get_one::<String>("tag").map(String::as_str),
                )?;
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    &request.name,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(&request.name, &dirs)?;
                installer
                    .install(
                        &plugin,
                        &client,
                        request.tag.as_deref(),
                        sub.get_one::<String>("target").map(String::as_str),
                        dry_run,
                    )
                    .await?;
            }
        }
        Some(("update", sub)) => {
            let dirs = resolved_plugin_dirs(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                force_refresh,
            )
            .await?;

            let update_all = sub.get_flag("all");
            let dry_run = sub.get_flag("dry-run");
            let package = sub.get_one::<String>("package");
            let requested_tag = sub.get_one::<String>("tag").map(String::as_str);
            let requested_target = sub.get_one::<String>("target").map(String::as_str);

            if update_all {
                if package.is_some()
                    || requested_tag.is_some()
                    || requested_target.is_some()
                {
                    anyhow::bail!(
                        "Use either `update <package> [--tag <tag>] [--target <triple>]` or `update --all`, not both"
                    );
                }
                let outdated = collect_outdated_packages(
                    &installer, &settings, &client, &dirs, None, true,
                )
                .await?;
                if outdated.is_empty() {
                    println!("All installed packages are up to date.");
                } else {
                    let installer = installer.clone();
                    let client = client.clone();
                    let settings = settings.clone();
                    let package_count = outdated.len();
                    let results = stream::iter(outdated.into_iter().map(|entry| {
                        let installer = installer.clone();
                        let client = client.clone();
                        let settings = settings.clone();
                        async move {
                            let package_name = entry.name.clone();
                            let package_dirs = match resolved_plugin_dirs_for_query(
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
                            let plugin =
                                match plugin::find_plugin(&entry.name, &package_dirs) {
                                    Ok(plugin) => plugin,
                                    Err(err) => return Err((package_name, err)),
                                };
                            match installer
                                .install(
                                    &plugin,
                                    &client,
                                    Some(&entry.latest_version),
                                    None,
                                    dry_run,
                                )
                                .await
                            {
                                Ok(()) => Ok(entry.name),
                                Err(err) => Err((entry.name, err)),
                            }
                        }
                    }))
                    .buffer_unordered(UPDATE_ALL_CONCURRENCY)
                    .collect::<Vec<_>>()
                    .await;

                    let mut succeeded = Vec::new();
                    let mut failed = Vec::new();
                    for result in results {
                        match result {
                            Ok(name) => succeeded.push(name),
                            Err((name, err)) => failed.push((name, err)),
                        }
                    }

                    println!(
                        "Update summary: {} succeeded, {} failed, {} total.",
                        succeeded.len(),
                        failed.len(),
                        package_count
                    );
                    if !succeeded.is_empty() {
                        println!("Succeeded: {}", succeeded.join(", "));
                    }
                    if !failed.is_empty() {
                        println!(
                            "Failed: {}",
                            failed
                                .iter()
                                .map(|(name, _)| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        let details = failed
                            .into_iter()
                            .map(|(name, err)| format!("{name}: {err}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        anyhow::bail!(
                            "One or more package updates failed during `update --all`:\n{}",
                            details
                        );
                    }
                }
            } else {
                let package = package.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Missing package name. Use `update <package> [--tag <tag>]` or `update --all`"
                    )
                })?;
                let request = parse_package_request(package, requested_tag)?;
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    &request.name,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(&request.name, &dirs)?;
                installer
                    .install(
                        &plugin,
                        &client,
                        request.tag.as_deref(),
                        requested_target,
                        dry_run,
                    )
                    .await?;
            }
        }
        Some(("uninstall", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            let dry_run = sub.get_flag("dry-run");
            let dirs = resolved_plugin_dirs_for_query(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                package,
                force_refresh,
            )
            .await?;
            let plugin = plugin::find_plugin(package, &dirs)?;
            installer.uninstall(&plugin, dry_run).await?;
        }
        Some(("plugins", sub)) => match sub.subcommand() {
            Some(("list", sub)) => {
                let json = sub.get_flag("json");
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
                let plugins = plugin::load_plugins_from_dirs(&dirs)?;
                print_available_plugins(&plugins, json);
            }
            Some(("search", sub)) => {
                let json = sub.get_flag("json");
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
                let query = sub
                    .get_one::<String>("query")
                    .map(|value| value.to_ascii_lowercase());
                let plugins = plugin::load_plugins_from_dirs(&dirs)?;
                let filtered: Vec<_> = plugins
                    .into_iter()
                    .filter(|plugin| matches_query(plugin, query.as_deref()))
                    .collect();
                print_available_plugins(&filtered, json);
            }
            Some(("info", sub)) => {
                let package = sub.get_one::<String>("package").unwrap();
                let dirs = resolved_plugin_dirs_for_query(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    package,
                    force_refresh,
                )
                .await?;
                let plugin = plugin::find_plugin(package, &dirs)?;
                print_plugin_info(&plugin);
            }
            Some(("index", sub)) => match sub.subcommand() {
                Some(("add", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.add(repo, &client).await?;
                    println!(
                        "Added remote plugin index '{}' on branch '{}'.",
                        index.repo, index.branch
                    );
                }
                Some(("list", sub)) => {
                    let json = sub.get_flag("json");
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let indexes = manager.list()?;
                    print_remote_indexes(&indexes, json);
                }
                Some(("enable", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.enable(repo)?;
                    println!("Enabled remote plugin index '{}'.", index.repo);
                }
                Some(("disable", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.disable(repo)?;
                    println!("Disabled remote plugin index '{}'.", index.repo);
                }
                Some(("remove", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.remove(repo)?;
                    println!("Removed remote plugin index '{}'.", index.repo);
                }
                Some(("sync", sub)) => {
                    let manager = remote_index::RemoteIndexManager::new()?;
                    if sub.get_flag("all") {
                        if sub.get_one::<String>("repo").is_some() {
                            anyhow::bail!(
                                "Use either `plugins index sync <repo>` or `plugins index sync --all`, not both"
                            );
                        }
                        let indexes = manager.sync_all_indexes(&client).await?;
                        if indexes.is_empty() {
                            println!("No enabled remote plugin indexes to sync.");
                        } else {
                            println!("Synced {} remote plugin index(es).", indexes.len());
                        }
                    } else {
                        let repo = sub.get_one::<String>("repo").ok_or_else(|| {
                            anyhow::anyhow!(
                                "Missing repo. Use `plugins index sync <owner>/<repo>` or `plugins index sync --all`"
                            )
                        })?;
                        let index = manager.sync_one(repo, &client).await?;
                        println!(
                            "Synced remote plugin index '{}' on branch '{}'.",
                            index.repo, index.branch
                        );
                    }
                }
                Some(("promote", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.promote(repo)?;
                    println!(
                        "Promoted remote plugin index '{}' in resolution order.",
                        index.repo
                    );
                }
                Some(("demote", sub)) => {
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.demote(repo)?;
                    println!(
                        "Demoted remote plugin index '{}' in resolution order.",
                        index.repo
                    );
                }
                Some(("pin", sub)) => {
                    let plugin_name = sub.get_one::<String>("plugin").unwrap();
                    let repo = sub.get_one::<String>("repo").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let index = manager.get_index(repo)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Remote plugin index '{}' is not configured",
                            repo
                        )
                    })?;
                    if !index.enabled {
                        anyhow::bail!(
                            "Remote plugin index '{}' is disabled. Enable it before pinning plugins to it.",
                            index.repo
                        );
                    }
                    manager.sync_one(&index.repo, &client).await?;
                    let cache_dir = manager.cache_dir_for_repo(&index.repo)?;
                    let cache_dir = cache_dir.to_string_lossy().to_string();
                    let plugin = plugin::find_plugin(plugin_name, &[cache_dir])?;
                    let pin = manager.pin_plugin_to_index(&plugin.name, &index.repo)?;
                    println!(
                        "Pinned plugin '{}' to remote plugin index '{}'.",
                        pin.plugin, pin.repo
                    );
                }
                Some(("unpin", sub)) => {
                    let plugin_name = sub.get_one::<String>("plugin").unwrap();
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let pin = manager.unpin_plugin(plugin_name)?;
                    println!("Removed remote plugin index pin for '{}'.", pin.plugin);
                }
                Some(("pins", sub)) => {
                    let json = sub.get_flag("json");
                    let manager = remote_index::RemoteIndexManager::new()?;
                    let pins = manager.list_plugin_pins()?;
                    print_plugin_index_pins(&pins, json);
                }
                _ => {}
            },
            _ => {}
        },
        Some(("outdated", sub)) => {
            let json = sub.get_flag("json");
            let package = sub.get_one::<String>("package").map(String::as_str);
            let dirs = resolved_plugin_dirs(
                &settings,
                &client,
                sub.get_one::<String>("plugins-dir"),
                force_refresh,
            )
            .await?;
            let filter_name = match package {
                Some(package) => {
                    let package_dirs = resolved_plugin_dirs_for_query(
                        &settings,
                        &client,
                        sub.get_one::<String>("plugins-dir"),
                        package,
                        force_refresh,
                    )
                    .await?;
                    let name = plugin::find_plugin(package, &package_dirs)?.name;
                    if !installer
                        .list_installed()?
                        .iter()
                        .any(|installed| installed.name == name)
                    {
                        anyhow::bail!("'{}' is not installed", name);
                    }
                    Some(name)
                }
                None => None,
            };
            let outdated = collect_outdated_packages(
                &installer,
                &settings,
                &client,
                &dirs,
                filter_name.as_deref(),
                false,
            )
            .await?;
            print_outdated_packages(&outdated, json);
        }
        Some(("doctor", _)) => {
            let checks =
                build_doctor_checks(&installer, &settings.default_plugin_dirs())?;
            print_doctor_checks(&checks);
        }
        Some(("list", sub)) | Some(("status", sub)) => {
            let json = sub.get_flag("json");
            let installed = installer.list_installed()?;
            if sub.get_flag("outdated") {
                let dirs = resolved_plugin_dirs(
                    &settings,
                    &client,
                    sub.get_one::<String>("plugins-dir"),
                    force_refresh,
                )
                .await?;
                let outdated = collect_outdated_packages(
                    &installer, &settings, &client, &dirs, None, false,
                )
                .await?;
                let rows = build_installed_status_rows(installed, &outdated);
                print_installed_status_rows(&rows, json);
            } else {
                print_installed_packages(&installed, json);
            }
        }
        Some(("verify", _)) => {
            let records = installer.audit()?;
            print_audit_records(&records, false);
        }
        Some(("audit", sub)) => {
            let json = sub.get_flag("json");
            let records = installer.audit()?;
            print_audit_records(&records, json);
        }
        Some(("history", sub)) => match sub.subcommand() {
            Some(("clear", clear_sub)) => {
                let package = clear_sub.get_one::<String>("package").map(String::as_str);
                let removed = installer.clear_history(package)?;
                if let Some(package) = package {
                    println!("Cleared {removed} history event(s) for '{}'.", package);
                } else {
                    println!("Cleared {removed} history event(s).");
                }
            }
            _ => {
                let json = sub.get_flag("json");
                let graph = sub.get_flag("graph");
                let package = sub.get_one::<String>("package").map(String::as_str);
                let limit = sub.get_one::<usize>("limit").copied();
                let events = installer.history_limited(package, limit)?;
                print_history(&events, graph, json);
            }
        },
        Some(("export", sub)) => {
            let format = parse_state_format(
                sub.get_one::<String>("format").map(String::as_str),
                sub.get_one::<String>("path").map(String::as_str),
            )?;
            let content = installer.export_state(format)?;
            if let Some(path) = sub.get_one::<String>("path") {
                std::fs::write(path, content)
                    .map_err(anyhow::Error::from)
                    .map_err(|err| {
                        err.context(format!("Failed to write state export to '{}'", path))
                    })?;
                println!("Exported state to '{}'.", path);
            } else {
                println!("{content}");
            }
        }
        Some(("restore", sub)) => {
            let path = sub.get_one::<String>("path").unwrap();
            let format = parse_state_format(
                sub.get_one::<String>("format").map(String::as_str),
                Some(path.as_str()),
            )?;
            let content = std::fs::read_to_string(path)
                .map_err(anyhow::Error::from)
                .map_err(|err| {
                    err.context(format!("Failed to read state backup from '{}'", path))
                })?;
            installer.restore_state(&content, format)?;
            println!("Restored state from '{}'.", path);
        }
        Some(("pin", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            installer.pin(package)?;
        }
        Some(("unpin", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            installer.unpin(package)?;
        }
        Some(("completions", sub)) => {
            use clap_complete::{Shell, generate};
            let shell_str = sub.get_one::<String>("shell").unwrap();
            let shell: Shell = shell_str
                .parse()
                .map_err(|_| anyhow::anyhow!("Unknown shell '{shell_str}'. Supported: bash, zsh, fish, elvish, powershell"))?;
            let mut stdout = std::io::stdout();
            generate(shell, &mut cli_app, "scpr", &mut stdout);
        }
        _ => {}
    }

    Ok(())
}

fn parse_package_request(package: &str, cli_tag: Option<&str>) -> Result<PackageRequest> {
    let (name, package_tag) = match package.split_once('@') {
        Some((name, tag)) if !name.is_empty() && !tag.is_empty() => {
            (name.to_string(), Some(tag.to_string()))
        }
        Some(_) => {
            anyhow::bail!(
                "Invalid package specifier '{package}'. Use <name> or <name>@<tag>"
            );
        }
        None => (package.to_string(), None),
    };

    if package_tag.is_some() && cli_tag.is_some() {
        anyhow::bail!("Use either <name>@<tag> or --tag <tag>, not both");
    }

    Ok(PackageRequest {
        name,
        tag: package_tag.or_else(|| cli_tag.map(str::to_string)),
    })
}

fn parse_state_format(
    format: Option<&str>,
    path: Option<&str>,
) -> Result<installer::StateFormat> {
    match format
        .or_else(|| {
            path.and_then(|path| Path::new(path).extension().and_then(|ext| ext.to_str()))
        })
        .unwrap_or("json")
    {
        "json" => Ok(installer::StateFormat::Json),
        "toml" => Ok(installer::StateFormat::Toml),
        other => Err(anyhow::anyhow!(
            "Unsupported state format '{other}'. Use json or toml."
        )),
    }
}

fn preferred_remote_pin_for_query(
    manager: &remote_index::RemoteIndexManager,
    query: &str,
) -> Result<Option<remote_index::PluginIndexPin>> {
    if let Some(pin) = manager.preferred_index_for_plugin(query)? {
        return Ok(Some(pin));
    }

    for pin in manager.list_plugin_pins()? {
        let cache_dir = manager.cache_dir_for_repo(&pin.repo)?;
        let cache_dir = cache_dir.to_string_lossy().to_string();
        for plugin in plugin::load_plugins_from_dir(&cache_dir)? {
            if plugin.name == query || plugin.alias.iter().any(|alias| alias == query) {
                return Ok(Some(pin));
            }
        }
    }

    Ok(None)
}

fn apply_preferred_remote_pin_to_dirs(
    manager: &remote_index::RemoteIndexManager,
    dirs: &mut Vec<String>,
    query: &str,
    has_extra_plugins_dir: bool,
) -> Result<()> {
    let Some(pin) = preferred_remote_pin_for_query(manager, query)? else {
        return Ok(());
    };

    let index = manager.get_index(&pin.repo)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Plugin '{}' is pinned to remote index '{}', but that index is no longer configured",
            pin.plugin,
            pin.repo
        )
    })?;

    if !index.enabled {
        anyhow::bail!(
            "Plugin '{}' is pinned to remote index '{}', but that index is disabled. Enable it or unpin the plugin.",
            pin.plugin,
            pin.repo
        );
    }

    let preferred_dir = manager.cache_dir_for_repo(&pin.repo)?;
    if let Some(position) = dirs
        .iter()
        .position(|dir| Path::new(dir) == preferred_dir.as_path())
    {
        let dir = dirs.remove(position);
        let insert_at = usize::from(has_extra_plugins_dir) + 1;
        dirs.insert(insert_at.min(dirs.len()), dir);
        Ok(())
    } else {
        anyhow::bail!(
            "Plugin '{}' is pinned to remote index '{}', but its cached plugin directory is unavailable at {}",
            pin.plugin,
            pin.repo,
            preferred_dir.display()
        )
    }
}

async fn collect_outdated_packages(
    installer: &installer::Installer,
    _settings: &settings::AppSettings,
    client: &github::GithubClient,
    dirs: &[String],
    filter_name: Option<&str>,
    skip_pinned: bool,
) -> Result<Vec<OutdatedPackage>> {
    use futures_util::future;
    use std::sync::Arc;

    let client = Arc::new(client);
    let all_installed = installer.list_installed()?;
    let filter_name = filter_name.map(str::to_string);

    // Build one future per non-pinned package, bounded to 8 concurrent requests.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(8));

    let futures: Vec<_> = all_installed
        .into_iter()
        .filter(|package| !skip_pinned || !package.pinned)
        .filter(|package| {
            filter_name
                .as_deref()
                .is_none_or(|name| package.name == name)
        })
        .map(|installed| {
            let client = Arc::clone(&client);
            let sem = Arc::clone(&semaphore);
            let dirs = dirs.to_vec();
            async move {
                let _permit = sem.acquire().await.ok()?;
                let manager = match remote_index::RemoteIndexManager::new() {
                    Ok(manager) => manager,
                    Err(err) => {
                        warn!("Skipping '{}' during update check: {err}", installed.name);
                        return None;
                    }
                };
                let mut package_dirs = dirs;
                if let Err(err) = apply_preferred_remote_pin_to_dirs(
                    &manager,
                    &mut package_dirs,
                    &installed.name,
                    false,
                ) {
                    warn!("Skipping '{}' during update check: {err}", installed.name);
                    return None;
                }
                let plugin = match plugin::find_plugin(&installed.name, &package_dirs) {
                    Ok(p) => p,
                    Err(err) => {
                        warn!("Skipping '{}' during update check: {err}", installed.name);
                        return None;
                    }
                };
                let (owner, repo) = plugin.github_repo()?;
                let release = client.get_latest_release(owner, repo).await.ok()?;
                if release.tag_name != installed.version {
                    Some(OutdatedPackage {
                        name: installed.name,
                        current_version: installed.version,
                        latest_version: release.tag_name,
                    })
                } else {
                    None
                }
            }
        })
        .collect();

    let mut outdated: Vec<OutdatedPackage> = future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect();
    outdated.sort_by(|l, r| l.name.cmp(&r.name));
    Ok(outdated)
}

fn build_installed_status_rows(
    installed: Vec<installer::InstalledPackage>,
    outdated: &[OutdatedPackage],
) -> Vec<InstalledPackageStatus> {
    let latest_versions = outdated
        .iter()
        .map(|package| (package.name.as_str(), package.latest_version.as_str()))
        .collect::<std::collections::HashMap<_, _>>();

    let mut rows = installed
        .into_iter()
        .map(|pkg| {
            let latest_version = latest_versions
                .get(pkg.name.as_str())
                .map(|value| (*value).to_string());
            let outdated = latest_version.is_some();
            InstalledPackageStatus {
                name: pkg.name,
                version: pkg.version.clone(),
                binary: pkg.binary,
                pinned: pkg.pinned,
                installed_at_unix: pkg.installed_at_unix,
                latest_version: Some(latest_version.unwrap_or(pkg.version)),
                outdated,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

fn build_doctor_checks(
    installer: &installer::Installer,
    plugin_dirs: &[String],
) -> Result<Vec<DoctorCheck>> {
    let mut checks = Vec::new();
    let local_bin = installer.local_bin_dir();
    let path_entries = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let path_has_local_bin = path_entries.iter().any(|entry| entry == local_bin);
    checks.push(DoctorCheck {
        name: "PATH",
        ok: path_has_local_bin,
        detail: format!("{} in PATH", local_bin.display()),
    });

    let state_file = installer.state_file_path();
    checks.push(DoctorCheck {
        name: "State File",
        ok: !state_file.exists() || state_file.is_file(),
        detail: format!("{}", state_file.display()),
    });

    let installed = installer.list_installed()?;
    let missing_binaries = installed
        .iter()
        .filter(|package| !installer.local_bin_dir().join(&package.binary).exists())
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Installed Binaries",
        ok: missing_binaries.is_empty(),
        detail: if missing_binaries.is_empty() {
            "All installed binaries are present".to_string()
        } else {
            format!("Missing binaries for: {}", missing_binaries.join(", "))
        },
    });

    let missing_man_pages = installed
        .iter()
        .flat_map(|package| {
            package.man_pages.iter().filter_map(|page| {
                let path = installer.local_man_dir().join(page);
                if path.exists() {
                    None
                } else {
                    Some(format!("{} ({})", page, package.name))
                }
            })
        })
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Man Pages",
        ok: missing_man_pages.is_empty(),
        detail: if missing_man_pages.is_empty() {
            "All recorded man pages are present".to_string()
        } else {
            format!("Missing man pages: {}", missing_man_pages.join(", "))
        },
    });

    let unreadable_plugin_dirs = plugin_dirs
        .iter()
        .filter_map(|dir| match plugin::load_plugins_from_dir(dir) {
            Ok(_) => None,
            Err(err) => Some(format!("{dir}: {err}")),
        })
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Plugin Dirs",
        ok: unreadable_plugin_dirs.is_empty(),
        detail: if unreadable_plugin_dirs.is_empty() {
            "Plugin directories are readable".to_string()
        } else {
            unreadable_plugin_dirs.join("; ")
        },
    });

    // Check whether the man page directory is reachable via MANPATH.
    let local_man = installer.local_man_dir();
    // Walk up two levels: man1 -> man -> share/man, then check that parent.
    let man_base = local_man
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let manpath_entries: Vec<std::path::PathBuf> = std::env::var("MANPATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let man_in_path = man_base.is_some_and(|base| manpath_entries.contains(&base));
    checks.push(DoctorCheck {
        name: "MANPATH",
        ok: man_in_path,
        detail: format!("{} in MANPATH", local_man.display()),
    });

    Ok(checks)
}

fn matches_query(plugin: &plugin::Plugin, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return true;
    };

    plugin.name.to_ascii_lowercase().contains(query)
        || plugin
            .alias
            .iter()
            .any(|alias| alias.to_ascii_lowercase().contains(query))
        || plugin
            .description
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains(query)
}

fn print_available_plugins(plugins: &[plugin::Plugin], json: bool) {
    if json {
        // Serialize a minimal view; Plugin itself derives nothing for JSON so we
        // build ad-hoc values.
        let values: Vec<serde_json::Value> = plugins
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "aliases": p.alias,
                    "description": p.description,
                    "location": p.location,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&values).unwrap());
        return;
    }

    if plugins.is_empty() {
        println!("No available plugins found.");
        return;
    }

    println!("{:<20} {:<18} Description", "Plugin", "Aliases");
    println!("{}", "-".repeat(80));
    for plugin in plugins {
        let aliases = if plugin.alias.is_empty() {
            "-".to_string()
        } else {
            plugin.alias.join(", ")
        };
        let description = plugin.description.as_deref().unwrap_or("-");
        println!("{:<20} {:<18} {}", plugin.name, aliases, description);
    }
}

fn print_installed_packages(installed: &[installer::InstalledPackage], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(installed).unwrap());
        return;
    }

    if installed.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!(
        "{:<20} {:<20} {:<20} {:<12} Installed",
        "Package", "Version", "Binary", "Pinned"
    );
    println!("{}", "-".repeat(85));
    for pkg in installed {
        let installed_date = pkg
            .installed_at_unix
            .map(|ts| {
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0)
                    .unwrap_or_default();
                dt.format("%Y-%m-%d").to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<20} {:<20} {:<12} {}",
            pkg.name,
            pkg.version,
            pkg.binary,
            if pkg.pinned { "yes" } else { "no" },
            installed_date
        );
    }
}

fn print_installed_status_rows(rows: &[InstalledPackageStatus], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }

    if rows.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!(
        "{:<20} {:<20} {:<20} {:<12} {:<20} {:<12} Latest",
        "Package", "Version", "Binary", "Pinned", "Installed", "Status"
    );
    println!("{}", "-".repeat(124));
    for row in rows {
        let installed_date = row
            .installed_at_unix
            .map(|ts| {
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0)
                    .unwrap_or_default();
                dt.format("%Y-%m-%d").to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<20} {:<20} {:<12} {:<20} {:<12} {}",
            row.name,
            row.version,
            row.binary,
            if row.pinned { "yes" } else { "no" },
            installed_date,
            if row.outdated { "outdated" } else { "current" },
            row.latest_version.as_deref().unwrap_or("-")
        );
    }
}

fn print_outdated_packages(packages: &[OutdatedPackage], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(packages).unwrap());
        return;
    }

    if packages.is_empty() {
        println!("All installed packages are up to date.");
        return;
    }

    println!("{:<20} {:<20} {:<20}", "Package", "Installed", "Latest");
    println!("{}", "-".repeat(60));
    for package in packages {
        println!(
            "{:<20} {:<20} {:<20}",
            package.name, package.current_version, package.latest_version
        );
    }
}

fn print_doctor_checks(checks: &[DoctorCheck]) {
    let failed = checks.iter().any(|check| !check.ok);
    for check in checks {
        let status = if check.ok { "[OK]" } else { "[FAIL]" };
        println!("{:<14} {:<6} {}", check.name, status, check.detail);
    }

    if failed {
        println!("Doctor found one or more issues.");
    } else {
        println!("Doctor did not find any issues.");
    }
}

fn print_audit_records(records: &[installer::AuditRecord], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(records).unwrap());
        return;
    }

    if records.is_empty() {
        println!("No packages installed.");
        return;
    }

    println!("{:<20} {:<12} Details", "Package", "Status");
    println!("{}", "-".repeat(90));
    for record in records {
        let status = match record.status {
            installer::AuditStatus::Ok => "[OK]",
            installer::AuditStatus::Modified => "[MODIFIED]",
            installer::AuditStatus::Missing => "[MISSING]",
            installer::AuditStatus::Untracked => "[UNTRACKED]",
        };
        println!("{:<20} {:<12} {}", record.package, status, record.detail);
        println!("  {}", record.binary_path.display());
    }

    let modified = records
        .iter()
        .filter(|record| {
            matches!(
                record.status,
                installer::AuditStatus::Modified | installer::AuditStatus::Missing
            )
        })
        .count();
    if modified == 0 {
        println!("Audit complete: no modified installed binaries detected.");
    } else {
        println!("Audit complete: {modified} package(s) need attention.");
    }
}

fn print_history(events: &[installer::HistoryEvent], graph: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(events).unwrap());
        return;
    }

    if events.is_empty() {
        println!("No package history recorded yet.");
        return;
    }

    if graph {
        use std::collections::BTreeMap;
        let mut by_package: BTreeMap<&str, Vec<&installer::HistoryEvent>> =
            BTreeMap::new();
        for event in events {
            by_package.entry(&event.package).or_default().push(event);
        }

        for (package, events) in by_package {
            println!("{package}");
            for event in events {
                let timestamp = human_timestamp(event.timestamp_unix);
                let marker = match event.action {
                    installer::HistoryAction::Installed => "+",
                    installer::HistoryAction::Updated => "~",
                    installer::HistoryAction::Removed => "-",
                    installer::HistoryAction::Pinned => "P",
                    installer::HistoryAction::Unpinned => "U",
                };
                println!("  {} {} {}", timestamp, marker, history_summary(event));
            }
        }
        return;
    }

    println!("{:<20} {:<19} {:<10} Details", "Package", "When", "Action");
    println!("{}", "-".repeat(90));
    for event in events {
        println!(
            "{:<20} {:<19} {:<10} {}",
            event.package,
            human_timestamp(event.timestamp_unix),
            history_action_label(&event.action),
            history_summary(event)
        );
    }
}

fn history_action_label(action: &installer::HistoryAction) -> &'static str {
    match action {
        installer::HistoryAction::Installed => "installed",
        installer::HistoryAction::Updated => "updated",
        installer::HistoryAction::Removed => "removed",
        installer::HistoryAction::Pinned => "pinned",
        installer::HistoryAction::Unpinned => "unpinned",
    }
}

fn history_summary(event: &installer::HistoryEvent) -> String {
    match event.action {
        installer::HistoryAction::Installed => {
            format!(
                "installed {}",
                event.version.as_deref().unwrap_or("unknown")
            )
        }
        installer::HistoryAction::Updated => format!(
            "{} -> {}",
            event.from_version.as_deref().unwrap_or("unknown"),
            event.to_version.as_deref().unwrap_or("unknown")
        ),
        installer::HistoryAction::Removed => {
            format!("removed {}", event.version.as_deref().unwrap_or("unknown"))
        }
        installer::HistoryAction::Pinned | installer::HistoryAction::Unpinned => event
            .detail
            .clone()
            .unwrap_or_else(|| history_action_label(&event.action).to_string()),
    }
}

fn human_timestamp(timestamp_unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp_unix as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn print_plugin_info(plugin: &plugin::Plugin) {
    println!("Name: {}", plugin.name);
    println!(
        "Aliases: {}",
        if plugin.alias.is_empty() {
            "-".to_string()
        } else {
            plugin.alias.join(", ")
        }
    );
    println!(
        "Description: {}",
        plugin.description.as_deref().unwrap_or("-")
    );
    println!("Source: {}", plugin.location);
    println!("Asset Pattern: {}", plugin.asset_pattern);
    println!(
        "Checksum Pattern: {}",
        plugin
            .checksum_asset_pattern
            .as_deref()
            .unwrap_or("GitHub digest only")
    );
    println!("Binary Path: {}", plugin.binary);
    println!(
        "Man Pages: {}",
        plugin
            .man_pages
            .as_deref()
            .map(|items| items.join(", "))
            .unwrap_or_else(|| "-".to_string())
    );
    println!("Targets:");
    if let Some(targets) = &plugin.targets {
        let mut targets: Vec<_> = targets.iter().collect();
        targets.sort_by(|left, right| left.0.cmp(right.0));
        for (platform, target) in &targets {
            println!("  {platform} -> {target}");
        }
    } else {
        println!("  -");
    }

    // Show which target triple would be used for the running platform.
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let resolved = plugin
        .resolve_target(os, arch)
        .unwrap_or_else(|| "(not configured)".to_string());
    println!("Current Platform: {os}/{arch} -> {resolved}");
}

fn print_remote_indexes(indexes: &[remote_index::RemotePluginIndex], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(indexes).unwrap());
        return;
    }

    if indexes.is_empty() {
        println!("No remote plugin indexes configured.");
        return;
    }

    println!(
        "{:<4} {:<30} {:<16} {:<10} Last Synced",
        "Ord", "Repository", "Branch", "State"
    );
    println!("{}", "-".repeat(94));
    for (position, index) in indexes.iter().enumerate() {
        let last_synced = index
            .last_synced_unix
            .map(human_timestamp)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<4} {:<30} {:<16} {:<10} {}",
            position + 1,
            index.repo,
            index.branch,
            if index.enabled { "enabled" } else { "disabled" },
            last_synced
        );
    }

    println!();
    println!(
        "Earlier entries take precedence when multiple indexes provide the same plugin name."
    );
}

fn print_plugin_index_pins(pins: &[remote_index::PluginIndexPin], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(pins).unwrap());
        return;
    }

    if pins.is_empty() {
        println!("No plugin-specific remote index pins configured.");
        return;
    }

    println!("{:<20} Preferred Remote Index", "Plugin");
    println!("{}", "-".repeat(72));
    for pin in pins {
        println!("{:<20} {}", pin.plugin, pin.repo);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_package_request, parse_state_format};
    use crate::installer::StateFormat;

    #[test]
    fn test_parse_package_request_with_inline_tag() {
        let request = parse_package_request("ripgrep@15.1.0", None).unwrap();
        assert_eq!(request.name, "ripgrep");
        assert_eq!(request.tag.as_deref(), Some("15.1.0"));
    }

    #[test]
    fn test_parse_package_request_with_cli_tag() {
        let request = parse_package_request("ripgrep", Some("v15.1.0")).unwrap();
        assert_eq!(request.name, "ripgrep");
        assert_eq!(request.tag.as_deref(), Some("v15.1.0"));
    }

    #[test]
    fn test_parse_package_request_rejects_conflicting_tags() {
        let error = parse_package_request("ripgrep@15.1.0", Some("v15.1.0")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Use either <name>@<tag> or --tag <tag>, not both")
        );
    }

    #[test]
    fn test_parse_state_format_prefers_extension_when_flag_missing() {
        let format = parse_state_format(None, Some("backup.toml")).unwrap();
        assert!(matches!(format, StateFormat::Toml));
    }

    #[test]
    fn test_parse_state_format_defaults_to_json() {
        let format = parse_state_format(None, None).unwrap();
        assert!(matches!(format, StateFormat::Json));
    }
}
