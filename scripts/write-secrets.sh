#!/usr/bin/env bash
# Materialise ./secrets/* from BOX_TOKEN / BOX_VIEWER_PASSWORD (or .env) so Compose
# can mount them instead of putting the values in the container environment.
#
# Files are 0644 inside a 0700 directory on purpose: the directory is what keeps
# other host users out, while the file mode is what the container's uid 1000
# needs to read the bind mount regardless of which uid created it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "${ROOT}"

if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

if [[ -z "${BOX_TOKEN:-}" ]]; then
  echo "BOX_TOKEN is not set. Export it or add it to .env (the value is not printed)." >&2
  exit 1
fi
if [[ -z "${BOX_VIEWER_PASSWORD:-}" ]]; then
  echo "BOX_VIEWER_PASSWORD is not set. Export it or add it to .env (the value is not printed)." >&2
  exit 1
fi

mkdir -p "${ROOT}/secrets"
chmod 700 "${ROOT}/secrets"

write_secret() {
  local path="$1" value="$2"
  (
    umask 077
    printf '%s' "${value}" >"${path}"
  )
  chmod 644 "${path}"
}

write_secret "${ROOT}/secrets/box_token" "${BOX_TOKEN}"
write_secret "${ROOT}/secrets/box_viewer_password" "${BOX_VIEWER_PASSWORD}"

if [[ -n "${BOX_EGRESS_TUNNEL_BEARER:-}" ]]; then
  write_secret "${ROOT}/secrets/box_egress_tunnel_bearer" "${BOX_EGRESS_TUNNEL_BEARER}"
  echo "wrote secrets/box_token, secrets/box_viewer_password, secrets/box_egress_tunnel_bearer (values not printed)"
else
  echo "wrote secrets/box_token and secrets/box_viewer_password (values not printed)"
fi
