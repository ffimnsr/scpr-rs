# Plugin schemas

This directory documents the TOML format used by scpr plugins.

| File | Purpose |
| --- | --- |
| [`plugin.schema.json`](./plugin.schema.json) | Generated JSON Schema for plugin TOML files. Use it for editor validation and completion. Do not edit it by hand. |
| [`plugin.template.toml`](./plugin.template.toml) | Commented starter file showing the fields needed for a new plugin. |

## Creating a plugin

1. Copy `plugin.template.toml` into the plugin directory, for example `plugins/mytool.toml`.
2. Set `name`, `alias`, `description`, `location`, and the asset or build fields for the upstream project.
3. Map the platforms you support in `[plugin.targets]`.
4. Validate the result:

```bash
scpr plugins validate plugins/mytool.toml
scpr plugins test plugins/mytool.toml
```

`scpr plugins new <owner>/<repo>` can also generate a best-effort starter file from a GitHub repository.

## Validating plugins

Validate a single plugin file or every top-level TOML file in a plugin directory:

```bash
scripts/validate-plugins.sh plugins/mdbook.toml
scripts/validate-plugins.sh plugins
scripts/validate-plugins.sh          # defaults to plugins/
```

The script builds `target/debug/scpr` first unless `--no-build` is passed or `SCPR_BIN` points to an existing binary, then runs `scpr plugins validate` for each file.

## Regenerating the schema

The schema is generated from the Rust types with `schemars`, so field changes in the parser are reflected automatically. Constraints that cannot be derived from the types (for example the `location` pattern) are added by `apply_schema_constraints` in `src/plugin.rs`.

```bash
cargo run -- plugins schema --output schemas/plugin.schema.json
# or
scripts/validate-plugins.sh --write-schema
```

Check that the committed schema is current (also covered by a unit test):

```bash
scripts/validate-plugins.sh --check-schema
```

The generated schema describes the TOML structure and key constraints. `scpr plugins validate` remains the source of truth for full semantic validation.

## Required fields

Every plugin must define:

- `name` — primary plugin identifier.
- `alias` — names accepted by plugin lookup; use `[]` when none are needed.
- `location` — `github:<owner>/<repo>`.
- `binary` — path to the executable inside the archive or source checkout.

A plugin must also define either `asset_pattern` (download a release) or `build_script` (build from source with `--build`).

## Editor integration

The schema is a standard JSON Schema (draft 2020-12). Editors with TOML schema support can validate a plugin by adding a schema directive at the top of the file:

```toml
#:schema ../schemas/plugin.schema.json
```

Adjust the relative path based on where the plugin file lives. The template uses `#:schema plugin.schema.json` because it sits next to the schema.

For Taplo, this can also be configured in a `taplo.toml` file:

```toml
[[rule]]
include = ["plugins/*.toml"]
schema.path = "schemas/plugin.schema.json"
```

## Template placeholders

The following placeholders are supported in `asset_pattern`, `checksum_asset_pattern`, `signature_asset_pattern`, `binary`, `man_pages`, `files.from`, `files.to`, and `completions.command`:

| Placeholder | Value |
| --- | --- |
| `{name}` | Plugin name. |
| `{version}` | Release tag with a leading `v` stripped. |
| `{tag}` | Release tag as returned by GitHub. |
| `{target}` | Target triple resolved from `[plugin.targets]`. |

Lifecycle hooks and completions additionally support `{install_dir}`, `{data_home}`, `{config_home}`, `{binary_path}`, and `{shell}` where applicable.
