# Build stage
FROM rust:1.75-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false proxy

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
