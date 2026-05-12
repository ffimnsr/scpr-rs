# Issues

- [ ] Add `post_uninstall` and `cleanup` fields on plugin schema
    - `post_uninstall` is non optional and would run after uninstall
    this would mostly contain removal of installed shell completions modified paths, modified shell rc `bashrc, zshrc, bash_profile, etc.`, any non user generated items
    - `cleanup` is optional and would contain removal of user generated data involving the cli tool
        - On `uninstall` if the plugin has a field `cleanup` present a prompt to ask user if wanting to delete user generated config and data.
- [ ] Add `--describe` flag on `audit` subcommand to describe what's been modified or tampered, whether its the executable fingperprint, ownership, permissions, etc.
- [ ] Update `install` subcommand so it would never run if the same version of cli is already installed, unless `--force`.
- [ ] Update `update` subcommand so it would never run if the same version of cli is already installed, unless `--force` is applied.
- [ ] Centralize the warning plugin is shadowed by earlier plugin directory, currently its on different subcommands. There is already an update on `update --all` that beautify this error.
- [ ] Add checks on `install`/`update` if the binary is already on the system not installed by `scpr`. Like the one installed by `cargo` which is on `~/.cargo/bin`
- [ ] Add `--build` flag on `install`/`update` to build repo
    - [ ] Add `build_script` on plugin schema to assist with building data.
    - Maybe add `post_build` as well to do things
    - Support would only be for rust/c/cpp repos for now.