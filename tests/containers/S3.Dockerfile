# Build the pinned official source when registry access to the MinIO release image is unavailable.
# This image is a CI fixture only.
FROM public.ecr.aws/docker/library/golang:1.24.8 AS builder
ARG MINIO_RELEASE=RELEASE.2025-09-07T16-13-09Z
ENV GOTOOLCHAIN=local GOMAXPROCS=2 GOFLAGS=-p=2
WORKDIR /build
RUN git clone --depth 1 --branch "$MINIO_RELEASE" https://github.com/minio/minio.git .
RUN go mod download \
  && mkdir /artifacts \
  && CGO_ENABLED=0 go build -trimpath -buildvcs=false -o /artifacts/minio .

FROM public.ecr.aws/docker/library/debian:bookworm-slim
COPY --from=builder /artifacts/minio /usr/local/bin/minio
COPY --from=builder /build/LICENSE /usr/share/doc/minio/LICENSE
RUN mkdir -p /data && chown 1000:1000 /data
ENV HOME=/data
USER 1000:1000
EXPOSE 9000
ENTRYPOINT ["/usr/local/bin/minio"]
CMD ["server", "/data", "--address", ":9000", "--console-address", ":9001", "--quiet"]
