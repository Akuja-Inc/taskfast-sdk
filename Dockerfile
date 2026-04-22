FROM debian:bookworm-slim

# VERSION is the semver string without the "taskfast-cli-v" prefix (e.g. "0.2.5").
# TARGETARCH is populated automatically by buildx per --platform.
ARG VERSION
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      jq \
      xz-utils \
 && rm -rf /var/lib/apt/lists/*

# Fetch the matching cargo-dist-produced tarball from the GitHub Release.
# release.yml builds native binaries for both Linux triples; re-compiling them
# here inside QEMU used to cost ~55 min on arm64 alone — this step takes seconds.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) TRIPLE=x86_64-unknown-linux-gnu ;; \
      arm64) TRIPLE=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    URL="https://github.com/Akuja-Inc/taskfast-cli/releases/download/taskfast-cli-v${VERSION}/taskfast-cli-${TRIPLE}.tar.xz"; \
    curl -fsSL --retry 3 --retry-delay 5 "$URL" -o /tmp/tf.tar.xz; \
    tar -xJf /tmp/tf.tar.xz -C /tmp; \
    install -m 0755 "/tmp/taskfast-cli-${TRIPLE}/taskfast" /usr/local/bin/taskfast; \
    rm -rf /tmp/tf.tar.xz "/tmp/taskfast-cli-${TRIPLE}"; \
    /usr/local/bin/taskfast --version

COPY skills/taskfast-agent /opt/taskfast-skills

# F10: drop root. The CLI needs no privileged capability at runtime —
# it reads a keystore file, writes `.taskfast/`, and talks HTTPS. A
# compromised binary running as uid 0 inside the container can
# `chmod 4755` its own copy on a bind-mount or tamper with any volume
# mounted with default perms. Running as an unprivileged uid both
# reduces the blast radius and lets operators `--read-only` mount the
# image with tight host-side ACLs.
RUN useradd --uid 1000 --user-group --create-home --shell /bin/bash taskfast \
 && mkdir -p /work \
 && chown taskfast:taskfast /work
USER taskfast:taskfast

WORKDIR /work
ENTRYPOINT ["taskfast"]
CMD ["--help"]
