> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Timber

Timber is a Linux IDE based on [Zed](https://github.com/zed-industries/zed), with its own application identity, launcher, user data, and icon.

### Linux installation

Download `timber-linux-x86_64.tar.gz`, `install.sh`, and `SHA256SUMS` from the [latest Timber release](https://github.com/Coding-M8s/timber-ide/releases/latest) into the same directory, then run:

```sh
sha256sum --ignore-missing --check SHA256SUMS
sh install.sh
```

Open **Timber** from your application launcher or run `~/.local/bin/timber`. The installer preserves existing Timber files as backups. Timber uses separate paths from Zed, including `~/.local/timber.app`, `~/.config/timber`, and `~/.local/share/timber`.

To build and install an optimized Linux desktop release from source:

```sh
./script/install-linux
```

The current packaging target is the native Linux archive. Flatpak packaging is not configured for Timber.

### Developing Timber

The upstream development documentation also applies to Timber's Zed base:

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
