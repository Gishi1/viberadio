# Contributing to VibeRadio

Thanks for taking the time to improve VibeRadio.

## Development

VibeRadio needs Rust 1.86 or newer, `pkg-config` and the ALSA development
headers. On Debian/Ubuntu:

```bash
sudo apt install build-essential pkg-config libasound2-dev \
  libxkbcommon-dev libwayland-dev libx11-dev libgl1-mesa-dev
```

With Nix:

```bash
nix develop
```

Build and test:

```bash
cargo build
cargo test
```

The default suite is offline. Tests that need outbound network are marked
`#[ignore]`; run them explicitly with `cargo test -- --ignored`.

## Pull requests

- Keep a change focused and explain the reason in the commit subject.
- Add or update tests for behaviour changes.
- Do not commit recordings, databases, API keys or generated Flatpak sources
  unless the dependency set changed.
- Run `cargo test --locked` before opening the pull request.

## Reporting bugs

Include the VibeRadio version, the station URL if it is relevant, and the
relevant log lines. Logs are written to the terminal/journal; the database and
recordings live in `~/.local/share/VibeRadio/`.
