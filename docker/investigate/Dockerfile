# STUDIO-1135: only this image runs untrusted investigation commands. Base digests and toolchain
# versions are pinned; rebuilding after a package disappears is an explicit operator action.
FROM node:22.16.0-bookworm-slim@sha256:048ed02c5fd52e86fda6fbd2f6a76cf0d4492fd6c6fee9e2c463ed5108da0e34 AS node
FROM rust:1.97.0-slim-bookworm@sha256:6d220bf85c74e842a79da63997af8d2e74455c0b8847d8bb3a5888572334991d AS tools
RUN apt-get update && apt-get install -y --no-install-recommends \
    git=1:2.39.5-0+deb12u3 ripgrep='13.0.0-4*' jq=1.6-2.1+deb12u2 \
    curl=7.88.1-10+deb12u15 pkg-config=1.8.1-1 libssl-dev=3.0.22-1~deb12u1 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/rust /repo /cache /scratch \
    && cp -a /usr/local/rustup/toolchains/*/. /opt/rust/ \
    && rm -rf /usr/local/rustup /usr/local/cargo \
    && chmod 1777 /tmp /scratch
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && chmod 1777 /cache
# Scratch resets inherited image ENV (including CARGO_HOME/RUSTUP_HOME). Actual compiler binaries
# above work without rustup or writable toolchain state. No image contains an operator credential.
FROM scratch
COPY --from=tools / /
ENV PATH=/opt/rust/bin:/usr/local/bin:/usr/bin:/bin HOME=/scratch
USER 1000:1000
WORKDIR /repo
ENTRYPOINT []
CMD ["sleep", "1800"]
