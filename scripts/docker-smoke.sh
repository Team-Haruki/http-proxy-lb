#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
IMAGE_TAG="http-proxy-lb:smoke"
CONTAINER_NAME="http-proxy-lb-smoke"

cleanup() {
  docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  rm -rf "${TMP_DIR}"
}

trap cleanup EXIT

cat >"${TMP_DIR}/config.yaml" <<'EOF'
listen: "0.0.0.0:8080"
admin_listen: "0.0.0.0:9090"
mode: round_robin
reload_interval_secs: 0
access_log: true
health_check:
  interval_secs: 30
  timeout_secs: 2
limits:
  max_connections: 32
  request_timeout_secs: 5
  shutdown_timeout_secs: 5
domain_policy:
  mode: whitelist
  domains: []
upstream: []
EOF

docker build -t "${IMAGE_TAG}" "${ROOT_DIR}"

docker run -d \
  --name "${CONTAINER_NAME}" \
  -p 18080:8080 \
  -p 19090:9090 \
  -v "${TMP_DIR}/config.yaml:/etc/http-proxy-lb/config.yaml:ro" \
  "${IMAGE_TAG}" >/dev/null

for _ in $(seq 1 20); do
  if curl -fsS "http://127.0.0.1:19090/health" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

curl -fsS "http://127.0.0.1:19090/health"
echo
curl -fsS "http://127.0.0.1:19090/status"
echo
