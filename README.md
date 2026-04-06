# scpr

[![Crates.io](https://img.shields.io/crates/v/scpr?style=flat-square)](https://crates.io/crates/scpr)
[![Crates.io](https://img.shields.io/crates/l/scpr?style=flat-square)](https://crates.io/crates/scpr)
[![Crates.io](https://img.shields.io/crates/d/scpr?style=flat-square)](https://crates.io/crates/scpr)

`scpr` installs and manages standalone CLI binaries from GitHub releases in your user space.

It is built for tools like `ripgrep`, `fd`, or other projects that ship release archives, and it focuses on a clean local workflow:

- installs into `~/.local/bin`
- verifies downloads with SHA-256
- keeps install state locally
- supports updates, pinning, audit, and history
- aims to stay friendly in day-to-day terminal use

Bundled plugins currently include:

- `ripgrep`
- `fd`
- `bat`
- `delta`
- `eza`
- `zoxide`
- `starship`
- `fzf`
- `skim`
- `hyperfine`
- `jq`
- `lsd`
- `mk`
- `midas`

## Quick Start

Install `scpr` with the official script:

```sh
curl -sSfL https://raw.githubusercontent.com/ffimnsr/scpr-rs/main/install.sh | sh
```

If you prefer Cargo:

```bash
cargo install scpr
```

Then install your first package:

```bash
scpr install ripgrep
scpr install ripgrep fd bat
```

## Why scpr

`scpr` sits between manual binary downloads and full system package managers.

Use it when you want:

- a simple per-user install path
- release-based installs from GitHub
- lightweight package metadata
- safer upgrades than ad hoc `curl | tar | cp`
- visibility into what was installed, when, and whether it changed later

## Installation

### Install using script

The installer downloads the latest GitHub release for your platform, extracts it, installs the binary into `~/.local/bin`, and copies docs/license files when available.

Recommended command:

```sh
curl -sSfL https://raw.githubusercontent.com/ffimnsr/scpr-rs/main/install.sh | sh
```

Helpful installer options:

```sh
curl -sSfL https://raw.githubusercontent.com/ffimnsr/scpr-rs/main/install.sh | sh -s -- --bin-dir ~/.local/bin
curl -sSfL https://raw.githubusercontent.com/ffimnsr/scpr-rs/main/install.sh | sh -s -- --arch x86_64-unknown-linux-musl
```

### Install from Cargo

```bash
cargo install scpr
```

### After install

Make sure `~/.local/bin` is on your `PATH`.

Example for POSIX shells:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

## Command Overview

### Install and update

Install the latest release:

```bash
scpr install ripgrep
scpr install ripgrep fd bat
scpr install ripgrep --target x86_64-unknown-linux-musl
```

Install a specific release:

```bash
scpr install ripgrep@15.1.0
scpr install ripgrep --tag 15.1.0
```

Update one package:

```bash
scpr update ripgrep
scpr update ripgrep --target aarch64-apple-darwin
```

Update everything except pinned packages:

```bash
scpr update --all
```

`update --all` runs upgrades with bounded parallelism, shows overall `N / total` progress as packages finish, and ends with a success/failure summary.

Preview changes without writing anything:

```bash
scpr install ripgrep --dry-run
scpr update --all --dry-run
scpr uninstall ripgrep --dry-run
```

Force-refresh remote plugin indexes instead of waiting for the cache TTL:

```bash
scpr --refresh install ripgrep
scpr --refresh plugins list
```

### Inspect installed packages

List installed packages:

```bash
scpr list
scpr status
scpr list --outdated
```

List packages with newer releases available:

```bash
scpr outdated
scpr outdated ripgrep
```

Emit JSON for scripting:

```bash
scpr list --json
scpr list --outdated --json
scpr outdated --json
```

### Audit and verification

Verify installed binaries against the checksums recorded at install time:

```bash
scpr verify
```

`verify` is a compatibility alias for `audit`, so both commands show the same checksum and drift report.

Audit the local install directory and detect drift:

```bash
scpr audit
scpr audit --json
```

Audit result meanings:

- `OK`: the local binary matches the recorded checksum
- `MODIFIED`: the binary exists but its contents changed
- `MISSING`: the binary no longer exists
- `UNTRACKED`: no checksum was recorded, so the binary cannot be verified

### History

View package movement over time:

```bash
scpr history
scpr history ripgrep
scpr history --limit 20
scpr history --graph
scpr history clear
scpr history clear ripgrep
```

History tracks:

- installs
- updates
- removals
- pin/unpin events

### Pinning

Pin a package so `update --all` skips it:

```bash
scpr pin ripgrep
scpr unpin ripgrep
```

### Plugin discovery

List available plugins:

```bash
scpr plugins list
```

Search available plugins:

```bash
scpr plugins search rip
```

Inspect a plugin definition:

```bash
scpr plugins info ripgrep
```

Generate a best-effort plugin skeleton from a GitHub repo:

```bash
scpr plugins new sharkdp/fd
scpr plugins new yourname/yourtool --stdout
scpr plugins new owner/repo --output ./plugins/yourtool.toml
```

`plugins new` inspects the latest GitHub release and writes a starter TOML. It is intentionally best-effort, so review the generated asset pattern, binary path, checksum source, and target mappings before committing it.

Manage remote GitHub-backed plugin indexes:

```bash
scpr plugins index add ffimnsr/scpr-plugins
scpr plugins index list
scpr plugins index pin ripgrep ffimnsr/scpr-plugins
scpr plugins index pins
scpr plugins index unpin ripgrep
scpr plugins index promote ffimnsr/scpr-plugins
scpr plugins index demote ffimnsr/scpr-plugins
scpr plugins index disable ffimnsr/scpr-plugins
scpr plugins index enable ffimnsr/scpr-plugins
scpr plugins index sync --all
scpr plugins index sync ffimnsr/scpr-plugins
scpr plugins index remove ffimnsr/scpr-plugins
```

Use an additional plugin directory:

```bash
scpr plugins list --plugins-dir /path/to/plugins
scpr install mytool --plugins-dir /path/to/plugins
```

Configuration sources:

- `--plugins-dir /path/to/plugins` for one-off overrides
- `SCPR_PLUGINS_DIR` with standard path separators for extra plugin directories
- `SCPR_BIN_DIR` to override the binary install directory
- `~/.config/scpr/config.toml` for persistent settings such as `install_dir`, `man_dir`, `plugin_dirs`, and `index_ttl_secs`

Remote index notes:

- multiple remote indexes are supported
- only enabled indexes are used during normal plugin resolution
- remote indexes must currently be GitHub repositories
- `scpr` syncs plugin TOML files from `plugins/*.toml` in those repositories
- remote index syncs are cached for 10 minutes by default; use `--refresh` to bypass the TTL
- when the same plugin name exists in multiple indexes, the earlier index in `plugins index list` wins
- use `plugins index promote` and `plugins index demote` to change precedence without removing an index
- use `plugins index pin <plugin> <owner>/<repo>` to make one plugin prefer a specific remote index
- plugin source pins are respected by install, update, uninstall, `plugins info`, `outdated`, and `update --all`

### Create a Remote Plugin Index Repo

`scpr` expects a GitHub repository with plugin files stored under `plugins/*.toml`.

Minimal layout:

```text
my-scpr-index/
  plugins/
    ripgrep.toml
    fd.toml
    bat.toml
  README.md
```

Example `plugins/ripgrep.toml`:

```toml
[plugin]
name = "ripgrep"
alias = ["rg", "ripgrep"]
description = "A fast line-oriented search tool"
location = "github:BurntSushi/ripgrep"
asset_pattern = "{name}-{version}-{target}.tar.gz"
checksum_asset_pattern = "{name}-{version}-{target}.tar.gz.sha256"
binary = "{name}-{version}-{target}/rg"
man_pages = ["{name}-{version}-{target}/doc/rg.1"]

[plugin.targets]
"linux-x86_64" = "x86_64-unknown-linux-musl"
"linux-aarch64" = "aarch64-unknown-linux-musl"
"macos-x86_64" = "x86_64-apple-darwin"
"macos-aarch64" = "aarch64-apple-darwin"
```

Suggested workflow:

1. Create a GitHub repo such as `yourname/scpr-plugins`.
2. Add one or more plugin TOML files under the repo's `plugins/` directory.
3. Commit and push the repository.
4. Add it locally with `scpr plugins index add yourname/scpr-plugins`.
5. Confirm discovery with `scpr plugins index sync --all` and `scpr plugins list`.

Practical tips:

- keep plugin filenames stable and descriptive, such as `ripgrep.toml`
- store only plugin definitions in the repo; `scpr` does not need generated metadata files
- test a plugin locally first with `scpr plugins info <name>` before depending on it remotely
- if two indexes define the same plugin name, precedence still follows `plugins index list`

### Diagnostics and shell integration

Check local setup:

```bash
scpr doctor
```

`doctor` reports the problem and suggests a fix when it finds issues like a missing `PATH` entry, missing man-page search path, unreadable plugin directories, or recorded files that have drifted from local state.

Adjust CLI verbosity without setting `RUST_LOG` manually:

```bash
scpr -q list
scpr -v install ripgrep
scpr -vv plugins list
```

Export or restore state when moving to a new machine:

```bash
scpr export backup.json
scpr export backup.toml --format toml
scpr restore backup.json
```

Generate shell completions:

```bash
scpr completions bash
scpr completions zsh
scpr completions fish
```

## Storage Layout

By default `scpr` uses:

- binaries: `~/.local/bin`
- man pages: `~/.local/share/man/man1`
- state file: `~/.local/share/scpr/state.toml`
- user plugin definitions: `~/.local/share/scpr/plugins`
- local development plugin definitions: `./plugins`

Persistent configuration lives at:

- config file: `~/.config/scpr/config.toml`
- remote index config: `~/.local/share/scpr/remote-indexes.toml`

Example config:

```toml
install_dir = "/home/alice/.local/bin"
man_dir = "/home/alice/.local/share/man/man1"
plugin_dirs = ["/home/alice/.config/scpr/plugins", "/opt/scpr/plugins"]
index_ttl_secs = 600
```

## Plugin Format

Plugins are TOML files with a `[plugin]` table.

Example:

```toml
[plugin]
name = "ripgrep"
alias = ["rg", "ripgrep"]
description = "A fast line-oriented search tool"
location = "github:BurntSushi/ripgrep"
asset_pattern = "{name}-{version}-{target}.tar.gz"
checksum_asset_pattern = "{name}-{version}-{target}.tar.gz.sha256"
binary = "{name}-{version}-{target}/rg"
man_pages = ["{name}-{version}-{target}/doc/rg.1"]

[plugin.targets]
"linux-x86_64" = "x86_64-unknown-linux-musl"
"linux-aarch64" = "aarch64-unknown-linux-musl"
"macos-x86_64" = "x86_64-apple-darwin"
"macos-aarch64" = "aarch64-apple-darwin"
```

Supported template placeholders:

- `{name}`
- `{tag}`
- `{version}`
- `{target}`

## Safety Model

`scpr` currently verifies release downloads with SHA-256 using either:

- GitHub asset digest metadata
- a plugin-configured checksum sidecar asset such as `*.sha256`

It also:

- stages binary/man page replacements before swapping them into place
- uses a lock file to reduce concurrent state-write races
- keeps package metadata and movement history in local state

It does not currently verify signatures beyond checksums, and it assumes plugin definitions point to the correct release assets.

## Development

Useful local checks:

```bash
cargo fmt
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Recent test coverage includes:

- plugin parsing and target/template resolution
- checksum parsing and validation helpers
- lock-file lifecycle for concurrent state protection
- installer file commit behavior
- uninstall cleanup for tracked files and state
- audit detection for modified binaries
- package history recording for pin/remove actions
- package request parsing for `name@tag` and `--tag`

## Current Limitations

- GitHub releases are the only supported source today.
- `update --all` targets the latest release for each non-pinned package.
- plugin definitions are still intentionally simple and tuned for common release archive layouts.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
