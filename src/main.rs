use anyhow::Result;
use clap::{Arg, Command};
use tracing_subscriber::EnvFilter;

mod github;
mod installer;
mod plugin;

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// Return the default list of directories to search for plugin TOML files.
///
/// Order:
/// 1. `~/.local/share/scarper/plugins/` (user-installed plugins)
/// 2. `./plugins/` (development / local checkout)
fn default_plugin_dirs() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(
            home.join(".local/share/scarper/plugins")
                .to_string_lossy()
                .to_string(),
        );
    }
    dirs.push("plugins".to_string());
    dirs
}

#[tokio::main]
async fn main() -> Result<()> {
    // Use RUST_LOG if set, otherwise default to info-level output.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .init();

    let plugins_dir_arg = Arg::new("plugins-dir")
        .long("plugins-dir")
        .short('p')
        .help("Additional plugins directory to search")
        .action(clap::ArgAction::Set);

    let cli_app = Command::new("scarper")
        .bin_name("scarper")
        .version(PKG_VERSION)
        .about(PKG_DESCRIPTION)
        .arg_required_else_help(true)
        .propagate_version(true)
        .subcommand(
            Command::new("install")
                .about("Install the latest release of a package")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Plugin name or alias"),
                )
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
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(
            Command::new("update")
                .about("Update an installed package to the latest version")
                .arg(
                    Arg::new("package")
                        .required(true)
                        .help("Plugin name or alias"),
                )
                .arg(plugins_dir_arg.clone()),
        )
        .subcommand(Command::new("list").about("List all installed packages"))
        .subcommand(Command::new("status").about("Show installed packages (alias for list)"));

    let matches = cli_app.get_matches();

    let client = github::GithubClient::new(PKG_VERSION)?;
    let installer = installer::Installer::new()?;

    match matches.subcommand() {
        Some(("install", sub)) | Some(("update", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            let mut dirs = default_plugin_dirs();
            if let Some(extra) = sub.get_one::<String>("plugins-dir") {
                dirs.insert(0, extra.clone());
            }
            let plugin = plugin::find_plugin(package, &dirs)?;
            installer.install(&plugin, &client).await?;
        }
        Some(("uninstall", sub)) => {
            let package = sub.get_one::<String>("package").unwrap();
            let mut dirs = default_plugin_dirs();
            if let Some(extra) = sub.get_one::<String>("plugins-dir") {
                dirs.insert(0, extra.clone());
            }
            let plugin = plugin::find_plugin(package, &dirs)?;
            installer.uninstall(&plugin)?;
        }
        Some(("list", _)) | Some(("status", _)) => {
            let installed = installer.list_installed()?;
            if installed.is_empty() {
                println!("No packages installed.");
            } else {
                println!("{:<20} {:<20} {:<20}", "Package", "Version", "Binary");
                println!("{}", "-".repeat(60));
                for pkg in &installed {
                    println!(
                        "{:<20} {:<20} {:<20}",
                        pkg.name, pkg.version, pkg.binary
                    );
                }
            }
        }
        _ => {}
    }

    Ok(())
}
