# syntax=docker/dockerfile:1

# ---- build stage -----------------------------------------------------------
FROM golang:1.26-alpine AS build
WORKDIR /src

# Cache module downloads.
COPY go.mod go.sum ./
RUN go mod download

# Build a static binary (no CGO; pure-Go deps). The admin SPA is embedded via
# go:embed, so no Node toolchain is required.
COPY . .
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -ldflags="-s -w" \
    -o /out/copysyncd ./cmd/copysyncd

# Pre-create the data dir owned by the distroless nonroot uid (65532) so that a
# freshly-initialized named volume inherits writable ownership.
RUN mkdir -p /data && chown 65532:65532 /data

# ---- runtime stage ---------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/copysyncd /copysyncd
COPY --from=build --chown=65532:65532 /data /data
VOLUME ["/data"]
EXPOSE 8443
USER nonroot:nonroot
ENTRYPOINT ["/copysyncd"]
