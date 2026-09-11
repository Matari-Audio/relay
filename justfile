set shell := ["bash", "-euo", "pipefail", "-c"]

pnpm_version := "11.22.0"
buf_version := "1.72.0"
nextest_version := "0.9.143"
cargo_deny_version := "0.20.2"
typos_version := "1.49.0"

# Show available repository commands.
default:
    @just --list

# Install pinned developer tools and locked project dependencies; run no validation.
bootstrap: bootstrap-tools bootstrap-dependencies
    @echo "Bootstrap complete. Run 'just check', 'just test', or 'just contracts' separately."

# Install repository-pinned Rust CLI tools idempotently through Cargo.
bootstrap-tools:
    cargo install --locked --version {{ nextest_version }} cargo-nextest
    cargo install --locked --version {{ cargo_deny_version }} cargo-deny
    cargo install --locked --version {{ typos_version }} typos-cli

# Install the frozen web dependency graph. Rust commands resolve only from Cargo.lock.
bootstrap-dependencies:
    npx --yes pnpm@{{ pnpm_version }} install --frozen-lockfile

# Run repository policy, Rust, web, and contract validation.
check: rust-fmt rust-check rust-lint rust-deny web-typecheck contracts typos

# Run the repository test suites that currently exist.
test: rust-test web-test

# Lint and compile the V1 Protobuf contracts with the pinned Buf CLI.
contracts:
    cd proto && npx --yes @bufbuild/buf@{{ buf_version }} format --diff --exit-code
    cd proto && npx --yes @bufbuild/buf@{{ buf_version }} lint
    cd proto && npx --yes @bufbuild/buf@{{ buf_version }} build
    cd proto && npx --yes @bufbuild/buf@{{ buf_version }} generate
    git diff --exit-code -- crates/relay-protocol/src/generated packages/protocol/src/generated

# Check every target and feature of the Rust workspace.
rust-check:
    cargo check --locked --workspace --all-targets --all-features

# Run tests for the Rust workspace with the repository config.
rust-test:
    cargo nextest run --locked --workspace --all-targets --all-features

# Check Rust formatting without modifying files.
rust-fmt:
    cargo fmt --all -- --check

# Run Rust lints with warnings denied.
rust-lint:
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

# Check workspace-wide Rust dependency policy.
rust-deny:
    cargo deny check

# Run every JavaScript package's existing test script.
web-test:
    npx --yes pnpm@{{ pnpm_version }} -r --if-present run test

# Run every JavaScript package's existing typecheck script.
web-typecheck:
    npx --yes pnpm@{{ pnpm_version }} -r run typecheck

# Run every JavaScript package's existing build script.
web-build:
    npx --yes pnpm@{{ pnpm_version }} -r run build

# Check spelling across the repository.
typos:
    typos

# Connect / Stream session tests (native UDP, no billing).
session-test:
    cargo test -p relay-session --all-targets -- --test-threads=1

# Truce plugin (separate workspace; CLAP/VST3/standalone).
plugin-test:
    cargo test --manifest-path apps/plugin/Cargo.toml --all-targets

# Standalone Connect / Stream / Link CLIs.
connect-build:
    cargo build -p relay-connect -p relay-stream -p relay-link

# Build and install the CLAP and VST3 next to other user plugins.
plugin-install:
    cd apps/plugin && cargo truce build --clap --vst3
    # Bundles land in the cargo target dir, which CARGO_TARGET_DIR may move.
    bundles="$(cargo metadata --no-deps --format-version 1 \
      --manifest-path apps/plugin/Cargo.toml | jq -r .target_directory)/bundles"
    mkdir -p "${HOME}/.clap" "${HOME}/.vst3"
    cp -f "${bundles}/RELAY.clap" "${HOME}/.clap/RELAY.clap"
    rm -rf "${HOME}/.vst3/RELAY.vst3"
    cp -a "${bundles}/RELAY.vst3" "${HOME}/.vst3/RELAY.vst3"
    ls -lah "${HOME}/.clap/RELAY.clap" "${HOME}/.vst3/RELAY.vst3"

# Gate, then package this host's installers into dist/. See scripts/release.sh.
release *ARGS:
    ./scripts/release.sh {{ ARGS }}

# Publish everything staged in dist/ as a GitHub release.
release-publish TAG:
    ./scripts/release.sh publish {{ TAG }}

# Deploy the named-session Worker to relay.matari-audio.com.
link-deploy:
    cd apps/relay-web && wrangler deploy

# Rebuild session CLIs + plugin so you can test Connect / Loopback / web listen.
ready:
    cargo test -p relay-session --all-targets -- --test-threads=1
    cargo build --release -p relay-connect -p relay-stream -p relay-link
    cargo test --manifest-path apps/plugin/Cargo.toml --all-targets
    just plugin-install
