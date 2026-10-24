#!/bin/sh
# All-in-one entrypoint: provision the administrator PSK on first run, then exec the server.
set -eu

data_dir="${FX_DATA_DIR:-/data}"
db_path="${FX_DB:-${data_dir}/fx_sim.db}"
admin_key="${FX_ADMIN_KEY:-${data_dir}/admin-key.json}"
static_dir="${FX_STATIC_DIR:-/app/static}"
listen="${FX_LISTEN:-0.0.0.0:${FX_PORT:-8000}}"

mkdir -p "${data_dir}"

if [ ! -f "${admin_key}" ]; then
  mkdir -p "$(dirname "${admin_key}")"
  # fx-admin-key reads FX_ADMIN_PSK; it prints a PSK only when it had to invent one.
  generated="$(fx-admin-key "${admin_key}")"
  if [ -n "${generated}" ]; then
    printf '%s\n' \
      "[fx-sim] ==========================================================" \
      "[fx-sim] administrator PSK (shown once): ${generated}" \
      "[fx-sim] replace it: docker rm this container, delete ${admin_key}" \
      "[fx-sim] and start again (admin sessions are memory-only)" \
      "[fx-sim] =========================================================="
  else
    printf '%s\n' "[fx-sim] administrator PSK provisioned from FX_ADMIN_PSK"
  fi
fi

if [ "$#" -eq 0 ]; then
  set -- --listen "${listen}" --db "${db_path}" \
    --static-dir "${static_dir}" --admin-key "${admin_key}"
fi

if [ "$(id -u)" = "0" ]; then
  # A bind-mounted data dir arrives root-owned; hand it to the unprivileged user once.
  chown -R fxsim:fxsim "${data_dir}" 2>/dev/null || true
  chown fxsim:fxsim "${admin_key}" 2>/dev/null || true
  exec setpriv --reuid=fxsim --regid=fxsim --init-groups fx-sim-rust "$@"
fi

exec fx-sim-rust "$@"
