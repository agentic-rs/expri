FROM rust:1.89-slim-bookworm AS builder
RUN apt-get update \
  && apt-get install --yes --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross cmake \
  && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu
WORKDIR /build
COPY source/ .
ENV CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
  AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/build/target \
  cargo build --release --locked --target x86_64-unknown-linux-gnu --bin expri --jobs 2 \
  && mkdir /artifacts \
  && cp target/x86_64-unknown-linux-gnu/release/expri /artifacts/expri \
  && x86_64-linux-gnu-strip /artifacts/expri
FROM scratch
COPY --from=builder /artifacts/expri /artifacts/expri
