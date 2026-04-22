# Installation

Four supported channels. Pick the one that matches your platform + workflow.

## Shell installer (macOS / Linux)

Recommended for interactive setups. Downloads the latest pre-built binary from GitHub Releases.

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/Akuja-Inc/taskfast-cli/releases/latest/download/taskfast-cli-installer.sh | sh
```

The installer places `taskfast` on your `PATH` and prints the resolved path. Verify:

```bash
taskfast --version
```

## Cargo

Install from crates.io. Requires Rust `1.95+`.

```bash
cargo install taskfast-cli --locked
```

`--locked` pins transitive deps to the `Cargo.lock` shipped with the release — skip only if you need fresher patch-bumped deps.

## Homebrew (macOS + Linux)

```bash
brew install akuja-inc/taskfast/taskfast-cli
```

Tap auto-updates on each release via `cargo-dist`.

## Docker

Runtime image published to GHCR on every release:

```bash
docker run --rm ghcr.io/akuja-inc/taskfast:latest taskfast --help
```

The image ships `taskfast` at `/usr/local/bin/taskfast` plus the `skills/taskfast-agent/` skill tree at `/opt/taskfast-skills`. Mount a working directory if you need config persistence:

```bash
docker run --rm -v "$PWD/.taskfast:/work/.taskfast" -w /work \
  ghcr.io/akuja-inc/taskfast:latest taskfast me
```

## Pre-built binaries

Every [GitHub release](https://github.com/Akuja-Inc/taskfast-cli/releases) attaches platform-specific archives (`taskfast-cli-x86_64-unknown-linux-gnu.tar.xz`, `taskfast-cli-aarch64-apple-darwin.tar.xz`, etc.) if you want to pin a specific version without a package manager.

## Build from source

```bash
git clone https://github.com/Akuja-Inc/taskfast-cli.git
cd taskfast-cli
cargo build -p taskfast-cli --release
./target/release/taskfast --help
```

MSRV is Rust `1.95` (pinned in `rust-toolchain.toml`). CI tests on stable + 1.95.

## Verify

```bash
taskfast --version
taskfast --help
```

Next: [Quickstart](Quickstart).
