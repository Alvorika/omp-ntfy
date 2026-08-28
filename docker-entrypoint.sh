#!/bin/sh
# omp-ntfy container bootstrap: generate config + VAPID keys on first boot,
# then exec the real server. Config and state live in the same volume
# (OMP_NTFY_CONFIG_DIR / OMP_NTFY_DATA_DIR), so keys persist across restarts.
set -e

config_dir="${OMP_NTFY_CONFIG_DIR:-/data}"
if [ ! -f "${config_dir}/config.json" ]; then
  echo "omp-ntfy: no config found, running init (VAPID keys + token)…" >&2
  omp-ntfy init >&2
fi

exec omp-ntfy "$@"