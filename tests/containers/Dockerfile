FROM ghcr.io/astral-sh/uv:0.8.15 AS uv

FROM rust:1.89-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update \
  && apt-get install --yes --no-install-recommends cmake \
  && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY tests/container_workflow.rs tests/container_workflow.rs
COPY python/expri_metrics.py python/expri_metrics.py
COPY dashboard_web/index.html dashboard_web/login.html dashboard_web/styles.css dashboard_web/app.js dashboard_web/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  --mount=type=cache,target=/build/target \
  cargo build --release --locked --bin expri \
  && rm -f target/release/deps/container_workflow-* \
  && cargo test --release --locked --test container_workflow --no-run \
  && mkdir /artifacts \
  && cp target/release/expri /artifacts/expri \
  && find target/release/deps -maxdepth 1 -type f -name 'container_workflow-*' -executable \
    -exec cp '{}' /artifacts/expri-container-tests \; \
  && test -x /artifacts/expri-container-tests

FROM python:3.12-slim-bookworm AS common
COPY --from=uv /uv /uvx /usr/local/bin/
RUN apt-get update \
  && apt-get install --yes --no-install-recommends git openssh-client rsync util-linux \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --create-home --uid 1000 --user-group --shell /bin/bash tester \
  && mkdir -p /run/expri-ssh
COPY --from=builder /artifacts/expri /usr/local/bin/expri
COPY tests/containers/build_wheels.py tests/containers/fake_torch.py \
  tests/containers/fetch_helper_wheels.py /opt/expri-fixtures/scripts/
RUN python /opt/expri-fixtures/scripts/build_wheels.py /opt/expri-fixtures/wheels \
  && python /opt/expri-fixtures/scripts/fetch_helper_wheels.py /opt/expri-fixtures/wheels \
  && python /opt/expri-fixtures/scripts/build_wheels.py /opt/expri-fixtures/wheels \
    --index-only --index-root /opt/expri-fixtures \
  && rm -rf /opt/expri-fixtures/scripts
ENV UV_PYTHON_DOWNLOADS=never \
  UV_INDEX_URL=http://worker:8000/simple \
  UV_NO_PROGRESS=1 \
  PYTHONDONTWRITEBYTECODE=1
WORKDIR /home/tester

FROM common AS worker
RUN apt-get update \
  && apt-get install --yes --no-install-recommends openssh-server \
  && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_* \
  && python -c 'import secrets; print("tester:" + secrets.token_urlsafe(32))' | chpasswd
RUN uv pip install --system --no-index --find-links /opt/expri-fixtures/wheels \
  'torch==2.10.0+cu128' 'nvidia-cuda-runtime-cu12==12.8.90' 'nvidia-cublas-cu12==12.8.4.1'
RUN printf '#!/bin/sh\nset -eu\nif [ "${1:-}" = node ] && [ "${2:-}" = pull-prepare ]; then\n  exec /usr/local/bin/expri "$@"\nfi\nexit 1\n' > /usr/local/bin/expri-old \
  && chmod 755 /usr/local/bin/expri-old
COPY --chmod=755 tests/containers/worker-entrypoint.sh /usr/local/bin/worker-entrypoint
EXPOSE 22 8000
ENTRYPOINT ["/usr/local/bin/worker-entrypoint"]

FROM common AS host
RUN uv pip install --system --no-index --find-links /opt/expri-fixtures/wheels 'torch==2.9.0+cpu'
COPY --from=builder /artifacts/expri-container-tests /usr/local/bin/expri-container-tests
COPY --chmod=755 tests/containers/host-entrypoint.sh /usr/local/bin/host-entrypoint
ENV EXPRI_TEST_BIN=/usr/local/bin/expri
ENTRYPOINT ["/usr/local/bin/host-entrypoint"]

FROM common AS service
USER tester
ENTRYPOINT ["/usr/local/bin/expri", "service", "serve"]

# Independent test-only browser: worker/host runtime images stay browser-free.
FROM python:3.12-slim-bookworm AS browser
RUN apt-get update \
  && apt-get install --yes --no-install-recommends firefox-esr curl ca-certificates openssl \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --create-home --uid 1000 --user-group --shell /bin/bash tester
# Mozilla v0.36.0 release artifacts, pinned by SHA256 for both CI architectures.
RUN case "$(dpkg --print-architecture)" in \
    amd64) platform=linux64; digest=0bde38707eb0a686a20c6bd50f4adcc7d60d4f73c60eb83ee9e0db8f65823e04 ;; \
    arm64) platform=linux-aarch64; digest=b5fd13fc8f05ca99292700896d43fa986747a01b5fdc77f7bfc88a3707dc4241 ;; \
    *) exit 1 ;; \
  esac \
  && curl --fail --show-error --silent --location --max-time 90 \
    "https://github.com/mozilla/geckodriver/releases/download/v0.36.0/geckodriver-v0.36.0-${platform}.tar.gz" \
    --output /tmp/geckodriver.tar.gz \
  && printf '%s  /tmp/geckodriver.tar.gz\n' "$digest" | sha256sum --check \
  && tar --extract --gzip --file /tmp/geckodriver.tar.gz --directory /usr/local/bin \
  && rm /tmp/geckodriver.tar.gz
COPY tests/containers/browser_proxy.py tests/containers/browser_forms.py /opt/expri-browser/
ENV HOME=/home/tester PYTHONDONTWRITEBYTECODE=1
ENTRYPOINT ["python3", "/opt/expri-browser/browser_proxy.py"]
