# scpr

[![Crates.io](https://img.shields.io/crates/v/scpr?style=flat-square)](https://crates.io/crates/scpr)
[![Crates.io](https://img.shields.io/crates/l/scpr?style=flat-square)](https://crates.io/crates/scpr)
[![Crates.io](https://img.shields.io/crates/d/scpr?style=flat-square)](https://crates.io/crates/scpr)

`scpr` installs and manages standalone CLI binaries from GitHub releases into your local user directories.

It is designed for tools like `ripgrep` that publish prebuilt archives, and it aims to keep installs predictable:

- downloads are verified with SHA-256 before extraction
- binaries and man pages are staged and swapped into place atomically
- installed package metadata is tracked locally
- retries and timeouts are applied to GitHub requests

## What It Manages

By default `scpr` uses these paths:

- binaries: `~/.local/bin`
- man pages: `~/.local/share/man/man1`
- state: `~/.local/share/scpr/state.toml`
- user plugin definitions: `~/.local/share/scpr/plugins`
- repo/local plugin definitions during development: `./plugins`

## Installation

Install from crates.io:

```bash
cargo install scpr
```

Then make sure `~/.local/bin` is on your `PATH`.

## Quick Start

Install the latest release of a tool:

```bash
scpr install ripgrep
```

Install a specific release:

```bash
scpr install ripgrep@15.1.0
scpr install ripgrep --tag 15.1.0
```

List installed packages:

```bash
scpr list
```

Check for newer releases:

```bash
scpr outdated
```

Update one package or everything installed:

```bash
scpr update ripgrep
scpr update --all
```

Remove a package:

```bash
scpr uninstall ripgrep
```

## Plugin Commands

Inspect available plugin definitions:

```bash
scpr plugins list
scpr plugins search rip
scpr plugins info ripgrep
```

Use an additional plugin directory:

```bash
scpr plugins list --plugins-dir /path/to/plugins
scpr install mytool --plugins-dir /path/to/plugins
```

## Health Checks

Run a local health check:

```bash
scpr doctor
```

`doctor` currently checks:

- whether `~/.local/bin` is on `PATH`
- whether the state file path looks valid
- whether tracked binaries still exist
- whether tracked man pages still exist
- whether plugin directories are readable

## Plugin Format

Plugins are TOML files with a `[plugin]` table. Example:

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
"macos-aarch64" = "aarch64-apple-darwin"
```

Supported template placeholders:

- `{name}`
- `{tag}`
- `{version}`
- `{target}`

## Security Model

`scpr` currently trusts plugin definitions you provide and verifies release downloads with SHA-256 using either:

- GitHub release asset metadata when available
- a configured checksum sidecar asset such as `*.sha256`

It does not currently verify signatures beyond checksums, and it assumes the plugin definition points at the correct binary and checksum assets.

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
- package request parsing for `name@tag` and `--tag`

## Limitations

- GitHub releases are the only supported source today.
- `update --all` always targets the latest release for each installed package.
- Plugin definitions are intentionally simple and currently tuned for common release archive layouts.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
