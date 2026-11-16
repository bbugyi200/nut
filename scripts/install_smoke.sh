#!/usr/bin/env bash
set -euo pipefail
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
mkdir -p "$root/bin" "$root/home"
install -m 0755 "$1" "$root/bin/nut"
NUT_SMOKE_BINARY="$root/bin/nut" cargo test --locked --test completion install_smoke_all_visible_help
export HOME="$root/home" XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data"
export PATH="$root/bin:$PATH"
unset ZDOTDIR BASH_ENV ENV BASH_COMPLETION_USER_DIR
nut --version
for shell in zsh bash; do
    nut completion install "$shell" --target "$root/$shell"
done
nut completion status --json
nut completion uninstall
test ! -e "$root/zsh/_nut"
test ! -e "$root/bash/nut"
