# Multi-stage build of rs_database_connect for a chosen PHP version.
# Build: docker build --build-arg PHP_VERSION=8.3 -t rsdb-php:8.3 -f docker/php/Dockerfile .
ARG PHP_VERSION=8.3
ARG RUST_VERSION=1.85

FROM rust:${RUST_VERSION}-bookworm AS builder
ARG PHP_VERSION
ARG RSDB_FEATURES=postgres

RUN apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev pkg-config libssl-dev \
      "php${PHP_VERSION}-dev" "php${PHP_VERSION}-cli" \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
ENV PHP_CONFIG=/usr/bin/php-config
RUN cargo build --release --features "${RSDB_FEATURES}" \
 && cp target/release/librs_database_connect.so /tmp/rs_database_connect.so

FROM php:${PHP_VERSION}-cli-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/rs_database_connect.so /usr/local/lib/php/extensions/
RUN echo "extension=/usr/local/lib/php/extensions/rs_database_connect.so" \
      > /usr/local/etc/php/conf.d/rs_database_connect.ini \
 && echo "zend.exception_ignore_args=On" >> /usr/local/etc/php/conf.d/rs_database_connect.ini
CMD ["php", "-m"]
