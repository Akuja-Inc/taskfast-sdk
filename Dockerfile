FROM rust:1.75-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build -p taskfast-cli --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      jq \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/taskfast /usr/local/bin/taskfast
COPY client-skills/taskfast-agent /opt/taskfast-skills

WORKDIR /work
ENTRYPOINT ["taskfast"]
CMD ["--help"]
