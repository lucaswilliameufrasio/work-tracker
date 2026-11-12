# syntax=docker/dockerfile:1
# Production image. Build through compose.yaml, which supplies the sibling
# sqlite-replication checkout as the `sqlite-replication` build context.

FROM node:24-trixie-slim@sha256:af02d57b57719b9657a45199a79264a9437940cd55b3c7b3fc99615654f124a0 AS web
WORKDIR /src/work-tracker/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm build

FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS build
RUN apt-get update \
    && apt-get install --yes --no-install-recommends build-essential cmake perl \
    && rm -rf /var/lib/apt/lists/*
# Cargo.toml points at ../sqlite-replication, so keep the same layout.
COPY --from=sqlite-replication rust/ /src/sqlite-replication/rust/
WORKDIR /src/work-tracker
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
COPY migrations/ migrations/
COPY examples/ examples/
COPY --from=web /src/work-tracker/web/dist/ web/dist/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/work-tracker/target \
    cargo build --release --locked --bin puxiri \
    && cp target/release/puxiri /usr/local/bin/puxiri
# Empty, non-root-owned mount points: named volumes inherit this ownership.
RUN mkdir -p /out/data /out/var/tmp

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cb87acc574abe719b6e5c547384defa2f7b1f24af9605d3f618cf852495d8be2
COPY --from=build /usr/local/bin/puxiri /usr/local/bin/puxiri
COPY --from=build --chown=65532:65532 /out/data /data
COPY --from=build --chown=65532:65532 /out/var/tmp /var/tmp
USER 65532:65532
ENV PUXIRI_DATA_DIR=/data \
    PUXIRI_BIND=0.0.0.0:3000 \
    TMPDIR=/var/tmp
VOLUME ["/data", "/var/tmp"]
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/puxiri"]
CMD ["serve"]
