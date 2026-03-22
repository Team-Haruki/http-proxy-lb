# Build stage
FROM rust:alpine AS builder

WORKDIR /app

# Install build dependencies
RUN apk add --no-cache build-base

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build release binary
RUN cargo build --release --locked

# Runtime stage
FROM alpine:3.22

# Install runtime dependencies
RUN apk add --no-cache \
    ca-certificates \
    curl

# Create non-root user
RUN addgroup -S proxy && adduser -S -D -H -h /nonexistent -s /bin/false -G proxy proxy

# Copy binary from builder
COPY --from=builder /app/target/release/http-proxy-lb /usr/local/bin/

# Create config directory
RUN mkdir -p /etc/http-proxy-lb && chown proxy:proxy /etc/http-proxy-lb

# Switch to non-root user
USER proxy

# Default config location
ENV CONFIG_PATH=/etc/http-proxy-lb/config.yaml

# Expose proxy port and admin port
EXPOSE 8080 9090

# Health check using admin endpoint
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:9090/health || exit 1

# Run the proxy
ENTRYPOINT ["http-proxy-lb"]
CMD ["--config", "/etc/http-proxy-lb/config.yaml"]
