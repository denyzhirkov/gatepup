# syntax=docker/dockerfile:1

# ---- builder (static musl binary) ----
FROM rust:1.92-alpine AS builder
# build-base: gcc/musl-dev for the ring (TLS) C build; perl is occasionally
# needed by ring's asm generation.
RUN apk add --no-cache build-base perl
WORKDIR /src
COPY . .
RUN cargo build --release -p gatepup-cli

# ---- runtime (alpine) ----
FROM alpine:3.20 AS runtime

# ca-certificates: forward-compat (TLS upstreams); libcap: setcap for port 80.
# busybox provides wget for the HEALTHCHECK (no curl needed).
RUN apk add --no-cache ca-certificates libcap

COPY --from=builder /src/target/release/gatepup /usr/local/bin/gatepup
# Allow the non-root process to bind privileged port 80.
RUN setcap cap_net_bind_service=+ep /usr/local/bin/gatepup

# Default config baked in; override by mounting over /etc/gatepup/config.json,
# or run config-free via GATEPUP_* environment variables.
COPY config.docker.json /etc/gatepup/config.json

RUN addgroup -S gatepup && adduser -S -D -H -u 10001 -G gatepup gatepup \
    && chown -R gatepup:gatepup /etc/gatepup
USER gatepup

EXPOSE 80 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -q -O- http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["gatepup"]
CMD ["run", "--config", "/etc/gatepup/config.json"]
