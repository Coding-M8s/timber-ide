> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# TAXE

TAXE (Timber Agentic Experience Editor) is a Linux IDE based on [Zed](https://github.com/zed-industries/zed), with its own application identity, launcher, user data, and icon.

### Linux installation

Download `taxe-linux-x86_64.tar.gz`, `install.sh`, and `SHA256SUMS` from the [TAXE 1.29.0 release](https://github.com/Coding-M8s/timber-ide/releases/tag/taxe-v1.29.0) into the same directory, then run:

```sh
sha256sum --ignore-missing --check SHA256SUMS
sh install.sh
~/.local/bin/taxe --version
```

This release provides Linux x86_64 binaries built on Arch Linux and requires glibc 2.44 or newer. The installer also supports locally built aarch64 bundles; no aarch64 asset is published for this release.

The version output should begin with `TAXE 1.29.0`. Open **TAXE** from your application launcher or run `~/.local/bin/taxe`. The installer also verifies the archive checksum and bundled version. Downloads are pinned to this release rather than whichever release is marked latest.

TAXE uses `~/.local/taxe.app`, `~/.config/taxe`, `~/.local/share/taxe`, and `~/.local/state/taxe`. XDG configuration, data, and state directory overrides are respected. Set `TAXE_INSTALL_ROOT` to override `~/.local`, or `TAXE_BUNDLE_PATH` to install a downloaded archive with its adjacent `SHA256SUMS` file.

### Upgrading from Timber IDE

Close the old editor before installing. When the installer finds an old `timber.app/libexec/timber-editor` bundle, it copies existing editor configuration, data, and state into the corresponding TAXE directories if those destinations do not already exist. Original data and the old bundle are retained. Existing TAXE installations are backed up before replacement.

The installer retires the old `timber` launcher only if it is a symlink to that IDE bundle, and archives its matching desktop entry. It preserves unrelated Timber CLI binaries and symlinks. Open a fresh shell after installation to refresh command lookup and run `command -v timber` to check the separate worktree CLI.

### Building the Linux release

Run these commands yourself to compile and package the desktop release, then install it:

```sh
./script/bundle-linux --desktop-only --skip-sentry
TAXE_BUNDLE_PATH="$PWD/target/release/taxe-linux-$(uname -m).tar.gz" sh script/install.sh
~/.local/bin/taxe --version
```

With `CARGO_TARGET_DIR` set, use that directory instead of `target`. `./script/install-linux` combines packaging and installation. The bundle command produces the archive, installer, and `SHA256SUMS` in `target/release`. The release channel is `stable` and the editor version is `1.29.0`.

The supported packaging target is the native Linux archive. Flatpak packaging is not configured for TAXE. See [RELEASE_NOTES.md](RELEASE_NOTES.md) for the release validation and draft upload commands.

### Worktree sidebar

The sidebar uses the separate Timber Git-worktree CLI (`timber` on `PATH`) for inventory, New/Open, and Done operations. The editor installs only `taxe`, leaving Timber's command and shell completions available. To use a custom worktree CLI path:

```sh
TIMBER_WORKTREE_CLI=/path/to/worktree-cli taxe /path/to/repository
```

The backend must support `ls --json`, `new`/`open` IDE result files, and native `__ide-done` cleanup previews. The worktree CLI is not included in the desktop archive. Its `TIMBER_WORKTREE_CLI` override and `TIMBER_IDE_OPEN_*` terminal handoff protocol remain compatible. Refresh reloads its inventory; Done opens a cleanup review before removing a linked worktree. The sidebar remains available with AI disabled.

### Developing TAXE

The upstream development documentation also applies to TAXE's Zed base:

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
