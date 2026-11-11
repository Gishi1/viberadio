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
AppStream metadata.

Options:

| Flag | Effect |
| --- | --- |
| `--skip-tests` | Skip `cargo test` (CI already ran it). |
| `--tag` | Create the annotated `v<version>` tag. |
| `--tag --push` | Create and push the tag. |
| `--out DIR` | Write artifacts to another directory. |

## 3. Publish

### GitHub

Pushing a `v*` tag runs `.github/workflows/release.yml`. It builds the tarball
and checksums and attaches them to a GitHub release with generated notes.

### GitLab

Pushing a `v*` tag runs the `build` job in `.gitlab-ci.yml` and creates a GitLab
release with the tarball and checksum links.

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
