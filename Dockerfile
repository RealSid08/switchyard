# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot proposes updates.

FROM node:26-bookworm-slim@sha256:662933cf47f013bc8e4beb31a6116448427a82057ba7c42c97e4c5ba766504c2 AS ui
WORKDIR /app/ui
RUN npm install --global --no-fund --no-audit pnpm@12.6.0
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm build

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS rust
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY --from=ui /app/ui/dist ui/dist
RUN cargo build --release --locked

# Same Debian release as the build image, so the binary's glibc matches.
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --create-home --uid 10001 --user-group switchyard \
  && install -d -m 700 -o switchyard -g switchyard /home/switchyard/.local/share/switchyard
COPY --from=rust /app/target/release/switchyard /usr/local/bin/switchyard
USER switchyard
WORKDIR /home/switchyard
# A named volume copies this directory's owner and 0700 mode on first use.
VOLUME /home/switchyard/.local/share/switchyard
EXPOSE 7410
STOPSIGNAL SIGTERM
ENTRYPOINT ["switchyard"]
# Bind all interfaces inside the container; compose.yaml publishes on host loopback only.
CMD ["--host", "0.0.0.0"]
