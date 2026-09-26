FROM rust:1.98-bookworm AS build
WORKDIR /src
ARG FACTORY_BUILD_REVISION=unversioned
ENV FACTORY_BUILD_REVISION=$FACTORY_BUILD_REVISION
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim AS worker
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m worker && mkdir -p /work && chown worker:worker /work
COPY --from=build /src/target/release/factory-worker /usr/local/bin/factory-worker
USER 10001:10001
WORKDIR /work
ENV TMPDIR=/work
ENTRYPOINT ["factory-worker"]

FROM node:24-bookworm-slim AS node-runtime

FROM python:3.12-slim-bookworm AS openhands-worker
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git ripgrep tmux build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 10001 -m worker && mkdir -p /work && chown worker:worker /work
COPY harnesses/openhands/requirements.txt /opt/factory/requirements.txt
COPY harnesses/openhands/requirements.lock /opt/factory/requirements.lock
RUN pip install --no-cache-dir -r /opt/factory/requirements.lock && pip check
COPY harnesses/openhands/adapter.py /opt/factory/openhands_adapter.py
COPY harnesses/validate.py /opt/factory/validate.py
COPY --from=node-runtime /usr/local/bin/node /usr/local/bin/node
COPY --from=node-runtime /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
COPY --from=build /usr/local/rustup /usr/local/rustup
COPY --from=build /usr/local/cargo/bin/ /usr/local/bin/
COPY --from=build /src/target/release/factory-worker /usr/local/bin/factory-worker
USER 10001:10001
WORKDIR /work
ENV TMPDIR=/work PYTHONDONTWRITEBYTECODE=1
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/work/cargo
ENV OTEL_EXPORTER_OTLP_TRACES_ENDPOINT="https://cloud.langfuse.com/api/public/otel/v1/traces"
ENV OTEL_EXPORTER_OTLP_TRACES_PROTOCOL="http/protobuf"
ENTRYPOINT ["factory-worker"]

FROM debian:bookworm-slim AS server
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates docker.io awscli && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/factory-server /usr/local/bin/factory-server
WORKDIR /app
COPY config ./config
COPY workflows ./workflows
COPY prompts ./prompts
COPY web ./web
ENV FACTORY_LISTEN=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["factory-server"]
