# TAXE 1.29.0

TAXE (Timber Agentic Experience Editor) is the new name for Timber IDE. The Linux editor command is now `taxe`, so the separate `timber` worktree CLI and its shell completions can coexist with the editor.

- Based on the existing 1.29 integration, including the native worktree sidebar.
- Uses a separate TAXE application ID, desktop launcher, and user data paths.
- Pins the installer to `taxe-v1.29.0` and verifies archive checksums and bundle version.
- Copies existing Timber IDE settings and data when an old IDE bundle is found, preserving the originals and any existing TAXE settings.

## Maintainer validation

The author compiled the Linux x86_64 package and confirmed that the GUI and installation work. Automated verification passed for the real archive checksums, fresh and repeat installation, TAXE 1.29.0 CLI output, Bash/Zsh/Fish completions, desktop entries, dynamic libraries, and all 14 installer regression tests. The published x86_64 binary requires glibc 2.44 or newer; an aarch64 package has not been built.

To repeat the build and verification, run the following in the release worktree:

```sh
./script/bundle-linux --desktop-only --skip-sentry
python3 script/test-taxe-install.py
(cd target/release && sha256sum --check SHA256SUMS)
TAXE_BUNDLE_PATH="$PWD/target/release/taxe-linux-$(uname -m).tar.gz" sh script/install.sh
~/.local/bin/taxe --version
~/.local/bin/taxe --completions bash > /tmp/taxe-completions.bash
command -v timber
```

Use `CARGO_TARGET_DIR` instead of `target` if set. Confirm the installed CLI reports `TAXE 1.29.0`, the desktop launcher opens TAXE, `taxe` opens a project, the worktree sidebar calls the separate Timber CLI, and generated completions target `taxe`. On an existing Timber IDE installation, close the old editor before installation and confirm settings and workspaces are retained. The Python checks use fixture bundles and do not validate a compiled editor.

For Rust validation, run `cargo test -p sidebar command_tests` and `./script/clippy -p cli -p paths -p release_channel -p zed`. Workflow YAML mirrors were updated by hand to avoid compiling xtask; regenerate and check them with `cargo xtask workflows` before merging.

## Release upload

The native bundle command creates `taxe-linux-<architecture>.tar.gz`, `install.sh`, and `SHA256SUMS` in `target/release`. This release ships the verified x86_64 archive. If adding another architecture later, build and verify it first. Regenerate checksums covering all shipped archives and the installer:

```sh
(cd target/release && sha256sum taxe-linux-*.tar.gz install.sh > SHA256SUMS)
gh release create taxe-v1.29.0 --repo Coding-M8s/timber-ide --target issue-13-taxe-release --draft --title "TAXE 1.29.0" --notes-file RELEASE_NOTES.md target/release/taxe-linux-*.tar.gz target/release/install.sh target/release/SHA256SUMS
```

Push the reviewed release branch before creating the draft. Verify its assets match the installer and README references before publishing. The inherited upstream workflows still target Zed infrastructure; use the fork draft upload command above for this release.

Release Notes:

- Improved the Linux 1.29 release with the TAXE name, independent launcher, verified downloads, and migration from Timber IDE.
