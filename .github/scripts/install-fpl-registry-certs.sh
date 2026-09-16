#!/usr/bin/env bash
set -euo pipefail
: "${FPL_REGISTRY_DIRECT_HOST:=registry-direct.fpl.dev}"
: "${FPL_REGISTRY_CA_CERT:?set FPL_REGISTRY_CA_CERT}"
: "${FPL_REGISTRY_CLIENT_CERT:?set FPL_REGISTRY_CLIENT_CERT}"
: "${FPL_REGISTRY_CLIENT_KEY:?set FPL_REGISTRY_CLIENT_KEY}"
cert_dir="/etc/docker/certs.d/${FPL_REGISTRY_DIRECT_HOST}"
sudo install -d -m 0755 "${cert_dir}"
printf '%s\n' "${FPL_REGISTRY_CA_CERT}" | sudo tee "${cert_dir}/ca.crt" >/dev/null
printf '%s\n' "${FPL_REGISTRY_CLIENT_CERT}" | sudo tee "${cert_dir}/client.cert" >/dev/null
printf '%s\n' "${FPL_REGISTRY_CLIENT_KEY}" | sudo tee "${cert_dir}/client.key" >/dev/null
sudo chmod 0644 "${cert_dir}/ca.crt" "${cert_dir}/client.cert"
sudo chmod 0600 "${cert_dir}/client.key"
