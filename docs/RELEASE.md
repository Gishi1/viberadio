# Release checklist

VibeRadio releases are built from a clean tag. The same script runs locally
and in CI.

## 1. Prepare

- Update `version` in `Cargo.toml` and the `releases` entry in
  `flatpak/io.github.gishi1.VibeRadio.metainfo.xml`.
- Move the notes for the version into `CHANGELOG.md`.
- Run the offline suite and a release build:

  ```bash
  cargo test --locked
  CARGO_BUILD_JOBS=2 cargo build --release --locked
  ```

  Release builds of the egui/wgpu stack are memory-hungry. The release script
  defaults to two jobs; set `CARGO_BUILD_JOBS` explicitly on a larger machine.
  Do not re-enable `lto` or `codegen-units = 1` in the release profile without
  checking peak memory, or the OOM killer will take the build.

- Check the Flatpak manifest still builds (see below).

## 2. Build the artifacts

```bash
scripts/release.sh
```

This runs `cargo test --locked`, builds the release binary, and writes:

```
dist/
  viberadio-<version>-linux-<arch>/
  viberadio-<version>-linux-<arch>.tar.gz
  SHA256SUMS
```

The tarball contains the binary, README, LICENSE, desktop entry, icon and
AppStream metadata. macOS and Windows reuse the same build step with their own
packaging scripts:

| Script | Output |
| --- | --- |
| `scripts/release.sh` | Linux tarball with desktop integration files |
| `scripts/package-appimage.sh` | `viberadio-<version>-linux-<arch>.AppImage` |
| `scripts/package-macos.sh` | `viberadio-<version>-macos-<arch>.tar.gz` |
| `scripts/package-windows.ps1` | `viberadio-<version>-windows-x86_64.zip` |

Options:

| Flag | Effect |
| --- | --- |
| `--skip-tests` | Skip `cargo test` (CI already ran it). |
| `--tag` | Create the annotated `v<version>` tag. |
| `--tag --push` | Create and push the tag. |
| `--out DIR` | Write artifacts to another directory. |

## 3. Publish

### GitHub

Pushing a `v*` tag runs `.github/workflows/release.yml`. It builds Linux
x86_64 (tarball and AppImage), Windows x86_64, macOS Apple Silicon and macOS
Intel in parallel, generates one `SHA256SUMS` and attaches every archive to a
GitHub release with generated notes.

### GitLab

Pushing a `v*` tag runs the `build` job in `.gitlab-ci.yml` and creates a GitLab
release with the Linux tarball and checksum links. GitLab has no hosted macOS
runners, and its Windows runner does not ship a Rust toolchain, so the Windows
and macOS archives come from GitHub Actions.

### Flatpak

Build and install the local manifest:

```bash
flatpak-builder --user --install --force-clean build-dir \
  flatpak/io.github.gishi1.VibeRadio.yml
flatpak run io.github.gishi1.VibeRadio
```

The manifest builds offline from `flatpak/cargo-sources.json`. Regenerate that
file after a dependency change:

```bash
scripts/generate-cargo-sources.sh
```

For Flathub, the repository, screenshots and app id in the manifest and
metainfo must match the project's public home.

## 4. Verify

- Extract the tarball on a clean machine and start the binary.
- Confirm the desktop entry, icon and AppStream metadata validate:

  ```bash
  appstreamcli validate flatpak/io.github.gishi1.VibeRadio.metainfo.xml
  desktop-file-validate packaging/viberadio.desktop
  ```

- Confirm the SHA256SUMS match the uploaded artifact.
