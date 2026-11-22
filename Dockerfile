# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm AS builder

WORKDIR /app

# Copy workspace manifests and crates needed to build droidpath-mcp
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps/droidpath-desktop/src-tauri/Cargo.toml ./apps/droidpath-desktop/src-tauri/Cargo.toml

# Provide minimal stubs for the desktop workspace member so Cargo resolves the workspace
RUN mkdir -p apps/droidpath-desktop/src-tauri/src \
    && printf 'pub fn noop() {}\n' > apps/droidpath-desktop/src-tauri/src/lib.rs \
    && printf 'fn main() {}\n' > apps/droidpath-desktop/src-tauri/src/main.rs

RUN cargo build --release --locked -p droidpath-mcp

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates adb \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /home/droidpath droidpath

COPY --from=builder /app/target/release/droidpath-mcp /usr/local/bin/droidpath-mcp
COPY packages/droidpath-cli/assets/android-daemon.jar /usr/local/share/droidpath/android-daemon.jar

USER droidpath
WORKDIR /home/droidpath

ENTRYPOINT ["/usr/local/bin/droidpath-mcp"]
CMD ["--stdio", "--daemon-jar", "/usr/local/share/droidpath/android-daemon.jar"]
