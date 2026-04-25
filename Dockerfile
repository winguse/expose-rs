# ── Runtime image built from pre-compiled binaries ───────────────────────────
# Binaries are produced by the release CI job and placed into ./dist/ before
# `docker build` is invoked.  The TARGETARCH build-arg is injected automatically
# by Docker Buildx when building a multi-platform image (e.g. amd64, arm64).
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG TARGETARCH
COPY dist/expose-server-${TARGETARCH} /usr/local/bin/expose-server
COPY dist/expose-client-${TARGETARCH} /usr/local/bin/expose-client

# Default to the server binary; override with expose-client when using as a client.
CMD ["expose-server"]
