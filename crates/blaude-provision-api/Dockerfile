# The provisioning service.
#
# Two stages: build the Rust binary against Debian's OpenSSL, then put it in a
# runtime image that also carries the gcloud CLI and ssh — provisioning copies
# the server binary onto the new VM and runs scripts there, so it needs both.
#
# gcloud is NOT signed in here, and must never be. Cloud Run attaches a service
# account and gcloud reads its token from the metadata server on every call.
# That is the whole point of moving this off people's Macs: a human
# `gcloud auth login` expires, and a service account does not.

FROM rust:1-bookworm AS build
WORKDIR /src

# Cache dependency compilation: manifests first, then sources. The workspace
# root is needed because these crates are members of it.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY deploy ./deploy
# The root package's own source: the `blaude` server binary is a bin target of
# the workspace ROOT, not of a crate under crates/. Without this the build
# fails with "can't find bin `blaude`" only at the target-resolution step —
# after every dependency has already compiled.
COPY src ./src
# Root markdown and docs/: jcode-app-core's build script embeds README.md and
# every docs/*.md into the binary (the runtime's built-in documentation
# corpus), so their absence is a compile error twenty minutes into the build.
COPY *.md ./
COPY docs ./docs

# The service AND the server binary it installs on every new VM. Building
# `blaude` here — the container is the same linux/x86_64 as the team servers —
# means each deploy carries a server built from the same commit as the
# service. The Mac-side cache this replaces was re-pulled only daily, so a
# same-day cache from before a fix silently shipped the OLD server to every
# brand-new team while the fix looked deployed.
RUN cargo build --release -p blaude-provision-api --bin blaude-provision-api \
 && cargo build --release --bin blaude \
 && cargo build --release -p blaude-tools

FROM debian:bookworm-slim
# ca-certificates: HTTPS to Clerk and to the Google APIs.
# openssh-client:  `gcloud compute scp` / `ssh` shell out to it.
# python3:         the provisioning scripts read JSON on the remote host.
# curl + gnupg:    to install the gcloud CLI below.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg openssh-client python3 \
 && curl -fsSL https://packages.cloud.google.com/apt/doc/apt-key.gpg \
      | gpg --dearmor -o /usr/share/keyrings/cloud.google.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/cloud.google.gpg] https://packages.cloud.google.com/apt cloud-sdk main" \
      > /etc/apt/sources.list.d/google-cloud-sdk.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends google-cloud-cli \
 && apt-get purge -y gnupg \
 && apt-get autoremove -y \
 && rm -rf /var/lib/apt/lists/*

# gcloud writes its config and a generated SSH key under $HOME. Cloud Run's
# filesystem is read-only except /tmp, so point both there or every call fails
# on a config write nobody would think to look for.
ENV HOME=/tmp
ENV CLOUDSDK_CONFIG=/tmp/gcloud
# The remote SSH login for gcloud compute scp/ssh. The project uses metadata
# SSH keys, where gcloud derives the remote username from the process login —
# and this process is root, which gcloud will not use. Pinning it (the startup
# code also sets USER/LOGNAME to match) makes the login deterministic, and the
# engine authorizes this same name in each new VM's metadata.
ENV BLAUDE_SSH_LOGIN=blaude

COPY --from=build /src/target/release/blaude-provision-api /usr/local/bin/blaude-provision-api
# The server binaries, where the engine looks for them (BLAUDE_SERVER_BINARY_DIR).
COPY --from=build /src/target/release/blaude /opt/blaude-server/blaude-linux-x86_64
COPY --from=build /src/target/release/blaude-tools /opt/blaude-server/blaude-tools-linux-x86_64
ENV BLAUDE_SERVER_BINARY_DIR=/opt/blaude-server

# Cloud Run sets PORT; 8080 is its default and the local one.
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/blaude-provision-api"]
