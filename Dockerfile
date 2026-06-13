# Optional container build for wagw-shimmy. The PRIMARY fleet deploy is a native binary + systemd
# (see deploy/); this image is for local E2E and CI convenience only. It builds the shim alone —
# GOWA ships as a separately-built, pinned binary (see deploy/provision.sh).

# ---- build ----
FROM rust:1.95-slim AS build
WORKDIR /src
# Cache deps first.
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --bin wagw-shimmy

# ---- runtime ----
FROM debian:bookworm-slim
RUN useradd --system --no-create-home --shell /usr/sbin/nologin wagw
COPY --from=build /src/target/release/wagw-shimmy /usr/local/bin/wagw-shimmy
USER wagw
# Config is supplied via the environment (see .env.example). The shim binds 127.0.0.1 by default;
# override SHIM_BIND=0.0.0.0:8080 when running in a container network.
ENV SHIM_BIND=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/wagw-shimmy"]
