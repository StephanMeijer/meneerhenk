# syntax=docker/dockerfile:1

# henk is cross-compiled on the build platform so that an arm64 image can be
# produced on an amd64 builder without emulation. The two MCP servers Henk
# starts as child processes ship in the same image: github-mcp-server from
# its official image, @zereight/mcp-gitlab installed with npm. See README.

FROM --platform=$BUILDPLATFORM docker.io/library/rust:1.93-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31 AS build
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src
# The cross linker and C headers are only needed when the target differs from
# the builder; the bundled SQLite is C and needs the target's libc headers.
RUN if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      case "$TARGETARCH" in \
        arm64) pkgs="gcc-aarch64-linux-gnu libc6-dev-arm64-cross"; triple=aarch64-unknown-linux-gnu ;; \
        amd64) pkgs="gcc-x86-64-linux-gnu libc6-dev-amd64-cross"; triple=x86_64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
      esac \
      && apt-get update \
      && apt-get install -y --no-install-recommends $pkgs \
      && rm -rf /var/lib/apt/lists/* \
      && rustup target add "$triple"; \
    fi
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
COPY Cargo.toml Cargo.lock henk.example.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
         unset "CARGO_TARGET_$(echo "$triple" | tr '[:lower:]-' '[:upper:]_')_LINKER" "CC_$(echo "$triple" | tr '-' '_')"; \
       fi \
    && cargo build --release --locked --package henk --target "$triple" \
    && install -D -m 0755 "target/$triple/release/henk" /out/usr/local/bin/henk \
    && install -d -m 0750 -o 65532 -g 65532 /out/var/lib/henk

FROM --platform=$BUILDPLATFORM docker.io/library/node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c AS mcp-gitlab
RUN --mount=type=cache,target=/root/.npm \
    npm install --global --prefix /opt/mcp-gitlab --ignore-scripts --no-audit --no-fund \
      @zereight/mcp-gitlab@2.1.68

FROM ghcr.io/github/github-mcp-server:v1.14.0@sha256:7aaeeec9ae4fe9a736d100c1ff0798f3c219b5009e05f5d3945fcacb13cc196b AS mcp-github

# The target platform's Node binary, copied rather than installed. This stage
# only serves COPY, so it needs no emulation on a cross builder.
FROM docker.io/library/node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c AS node-runtime

# Distroless cc: glibc, libstdc++ (Node needs it), CA certificates, tzdata and
# a non-root user, nothing else. Chosen over distroless/nodejs because its
# Debian packages are newer and Node brings its own OpenSSL anyway.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
LABEL org.opencontainers.image.source="https://github.com/StephanMeijer/meneerhenk" \
      org.opencontainers.image.title="Meneer Henk" \
      org.opencontainers.image.description="Advisory code reviewer and issue planner for GitHub and GitLab"
COPY --from=build /out/ /
COPY --from=node-runtime /usr/local/bin/node /nodejs/bin/node
COPY --from=mcp-github /server/github-mcp-server /usr/local/bin/github-mcp-server
COPY --from=mcp-gitlab /opt/mcp-gitlab /opt/mcp-gitlab
# Child servers are found on PATH; the GitLab server is started as
# `node /opt/mcp-gitlab/lib/node_modules/@zereight/mcp-gitlab/build/index.js`
# because distroless has no /usr/bin/env for the package's shebang.
ENV PATH=/usr/local/bin:/nodejs/bin \
    HENK_LOG_JSON=1
WORKDIR /var/lib/henk
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/henk"]
CMD ["--config", "/etc/henk/henk.toml", "serve"]
