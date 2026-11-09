#!/usr/bin/env bash
# Fail if docs/openapi.yaml is missing advertised guest routes.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SPEC="${ROOT}/docs/openapi.yaml"

if [[ ! -f "${SPEC}" ]]; then
  echo "missing ${SPEC}" >&2
  exit 1
fi

need=(
  "/v1/health:"
  "/v1/ready:"
  "/v1/info:"
  "/v1/exec:"
  "/v1/screen:"
  "/v1/exec/{id}:"
  "/v1/exec/stream:"
  "/v1/files:"
  "/v1/files/raw:"
  "/v1/files/mkdir:"
  "/v1/files/rename:"
  "/v1/busy:"
  "/v1/shutdown:"
  "/v1/metrics:"
  "/v1/desktop:"
  "/v1/desktop/windows:"
  "/v1/screens:"
  "/v1/chrome:"
  "/v1/egress:"
  "/v1/cua/screenshot:"
  "/v1/cua/click:"
  "/v1/cua/press:"
  "/v1/cua/release:"
  "/v1/cua/mousedown:"
  "/v1/cua/mouseup:"
  "/v1/cua/recipe:"
  "/v1/cua/wait:"
  "/v1/cua/record/start:"
  "/v1/cua/record/stop:"
  "/v1/cua/tree:"
  "/v1/cua/element:"
)

missing=0
for p in "${need[@]}"; do
  if ! grep -q -- "${p}" "${SPEC}"; then
    echo "openapi missing path ${p}" >&2
    missing=1
  fi
done

if ! grep -q "settle" "${SPEC}"; then
  echo "openapi missing recipe settle" >&2
  missing=1
fi
if ! grep -q "StepObservation" "${SPEC}"; then
  echo "openapi missing recipe step observation" >&2
  missing=1
fi
if ! grep -q "reset_desktop" "${SPEC}"; then
  echo "openapi missing reset_desktop" >&2
  missing=1
fi
if ! grep -q "stale_ref" "${SPEC}"; then
  echo "openapi missing the element route's stale_ref" >&2
  missing=1
fi
if ! grep -q "x-request-id" "${SPEC}"; then
  echo "openapi missing x-request-id" >&2
  missing=1
fi
if ! grep -q "output_complete" "${SPEC}"; then
  echo "openapi missing exec output_complete" >&2
  missing=1
fi
if ! grep -q "ExecCancelResponse" "${SPEC}"; then
  echo "openapi missing DELETE /v1/exec/{id} response" >&2
  missing=1
fi

if ! grep -q "viewer_port" "${SPEC}"; then
  echo "openapi missing screen viewer_port" >&2
  missing=1
fi
if ! grep -q "voxl" "${SPEC}"; then
  echo "openapi missing the voxl viewer" >&2
  missing=1
fi
if grep -qiE "novnc|x11vnc|websockify" "${SPEC}"; then
  echo "openapi still names the old viewer" >&2
  missing=1
fi

if [[ "${missing}" != "0" ]]; then
  exit 1
fi
echo "OpenAPI presence OK"
