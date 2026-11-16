#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "usage: remote_ups.sh HOST status|battery|fixture UPS [OUTPUT]" >&2
  exit 2
fi

host=$1
action=$2
ups=$3
output=${4:-}
nut_options=(--color never)
if [[ -n "${NUT_CONFIG:-}" ]]; then
  nut_options+=(--config "$NUT_CONFIG")
fi
case "$action" in
  status|battery) ;;
  fixture)
    if [[ -z "$output" ]]; then
      echo "fixture requires an output path" >&2
      exit 2
    fi
    ;;
  *)
    echo "unsupported read action: $action" >&2
    exit 2
    ;;
esac

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
cargo build --locked --bin nut

local_port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
ssh -nNT \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 \
  -o ServerAliveCountMax=2 \
  -L "127.0.0.1:${local_port}:127.0.0.1:3493" \
  "$host" &
tunnel_pid=$!
temporary_output=""
cleanup() {
  if [[ -n "$temporary_output" ]]; then
    rm -f -- "$temporary_output"
  fi
  kill "$tunnel_pid" >/dev/null 2>&1 || true
  wait "$tunnel_pid" >/dev/null 2>&1 || true
}
trap cleanup EXIT

ready=false
for _ in {1..50}; do
  if ! kill -0 "$tunnel_pid" 2>/dev/null; then
    wait "$tunnel_pid" || true
    echo "SSH port forward exited before it became ready" >&2
    exit 1
  fi
  if (true >/dev/tcp/127.0.0.1/"$local_port") 2>/dev/null; then
    ready=true
    break
  fi
  sleep 0.1
done
if [[ "$ready" != true ]]; then
  echo "SSH port forward did not become ready" >&2
  exit 1
fi

case "$action" in
  status)
    cargo run --quiet --locked --bin nut -- "${nut_options[@]}" status \
      --ups "$ups" --upsd-address 127.0.0.1 --upsd-port "$local_port"
    ;;
  battery)
    cargo run --quiet --locked --bin nut -- "${nut_options[@]}" battery \
      --ups "$ups" --upsd-address 127.0.0.1 --upsd-port "$local_port"
    ;;
  fixture)
    temporary_output="${output}.tmp.$$"
    cargo run --quiet --locked --bin nut -- "${nut_options[@]}" --json status \
      --ups "$ups" --upsd-address 127.0.0.1 --upsd-port "$local_port" \
      >"$temporary_output"
    mv -- "$temporary_output" "$output"
    temporary_output=""
    ;;
esac
