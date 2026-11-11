# openab-shepherd image (linux/amd64, glibc). Build with:
#   docker build --platform linux/amd64 -t <image>:<version> .
#
# The image holds no token and no launch profiles. A pod mounts them through an
# init container that copies them into an emptyDir (see the README "Container image"
# section). The server binds 127.0.0.1 only; the pod's tailnet sidecar forwards to it.

# --- Build stage ---
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

# --- Runtime stage ---
FROM debian:trixie-slim

# git: the server runs `git rev-parse` at startup and refuses to start if git cannot run.
# tini: PID 1 reaps orphaned pane processes, which otherwise stay as zombies.
# ripgrep, procps: the same tools the openab kiro image gives kiro-cli.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git procps ripgrep tini unzip \
    && rm -rf /var/lib/apt/lists/*

# Install kiro-cli: same version, URLs and checksums as the openab kiro image
# (openab Dockerfile, kiro stage).
ARG KIRO_CLI_VERSION=3.1.0
ARG KIRO_SHA256_AMD64=6f43c9ca9811d1ef2d4b47148667243b5af2d4aaea7cdef3e9a6af1c10d4c8ba
ARG KIRO_SHA256_ARM64=744701a386299ecc562766c591c54850cfae95775c19c0d6e3f4b5eaf55f87b0
RUN ARCH=$(dpkg --print-architecture) && \
    if [ "$ARCH" = "arm64" ]; then \
      URL="https://prod.download.cli.kiro.dev/stable/${KIRO_CLI_VERSION}/kirocli-aarch64-linux.zip"; \
      SHA256="$KIRO_SHA256_ARM64"; \
    else \
      URL="https://prod.download.cli.kiro.dev/stable/${KIRO_CLI_VERSION}/kirocli-x86_64-linux.zip"; \
      SHA256="$KIRO_SHA256_AMD64"; \
    fi && \
    curl --proto '=https' --tlsv1.2 -sSf --retry 3 --retry-delay 5 "$URL" -o /tmp/kirocli.zip && \
    echo "$SHA256  /tmp/kirocli.zip" | sha256sum -c - && \
    unzip /tmp/kirocli.zip -d /tmp && \
    cp /tmp/kirocli/bin/* /usr/local/bin/ && \
    chmod +x /usr/local/bin/kiro-cli* && \
    rm -rf /tmp/kirocli /tmp/kirocli.zip

RUN useradd -m -s /bin/bash -u 1000 shepherd && \
    mkdir -p /home/shepherd/.local/share/kiro-cli /home/shepherd/.kiro && \
    chown -R shepherd:shepherd /home/shepherd
ENV HOME=/home/shepherd
WORKDIR /home/shepherd

COPY --from=builder /build/target/release/openab-shepherd /usr/local/bin/openab-shepherd

# No HEALTHCHECK: the only route is POST /mcp, and every request needs the bearer token.
USER 1000:1000

ENTRYPOINT ["tini", "--"]
CMD ["openab-shepherd", "--host", "127.0.0.1", "--port", "8765"]
