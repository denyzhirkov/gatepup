# syntax=docker/dockerfile:1

# ---- builder ----
FROM rust:1.92-slim-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p gatepup-cli

# ---- runtime ----
FROM debian:bookworm-slim AS runtime

# curl for the container HEALTHCHECK; libcap2-bin to grant port-80 binding to a
# non-root process; ca-certificates for forward-compat (TLS upstreams).
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/gatepup /usr/local/bin/gatepup
# Allow the non-root process to bind privileged port 80.
RUN setcap 'cap_net_bind_service=+ep' /usr/local/bin/gatepup

# Default config baked in; override by mounting over /etc/gatepup/config.json.
COPY config.docker.json /etc/gatepup/config.json

RUN useradd --system --uid 10001 --no-create-home gatepup \
    && chown -R gatepup:gatepup /etc/gatepup
USER gatepup

EXPOSE 80 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["gatepup"]
CMD ["run", "--config", "/etc/gatepup/config.json"]
