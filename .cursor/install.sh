#!/usr/bin/env bash
set -euo pipefail

# The default Cloud Agent image ships Rust 1.83, but this crate uses edition
# 2024 and declares rust-version = 1.97, so it cannot build on the default
# toolchain. Install and default to the current stable toolchain (>= 1.97).
rustup toolchain install stable --profile minimal
rustup component add --toolchain stable rustfmt clippy
rustup default stable

# The app loads .env from the working directory and requires a signing key and a
# notification-detail base URL. Generate a local development .env once when it is
# missing. Never overwrite an existing .env (it may hold real configuration).
if [ ! -f .env ]; then
  cp .env.example .env
  key="$(openssl rand 32 | base64 | tr '+/' '-_' | tr -d '=\n')"
  sed -i "s|^ALERT_SIGNING_KEY=.*|ALERT_SIGNING_KEY=${key}|" .env
  sed -i "s|^ALERT_DETAIL_BASE_URL=.*|ALERT_DETAIL_BASE_URL=http://127.0.0.1:30010|" .env
  # Local development only: accept the terms flag so subscribe endpoints are
  # testable. Real deployments must set this deliberately (see README.md).
  sed -i "s|^INSTANCE_TERMS_ACCEPTED=.*|INSTANCE_TERMS_ACCEPTED=true|" .env
fi

# Pre-build the binary so the first `cargo run` in the terminal starts quickly.
cargo build --bin disaster-alert
