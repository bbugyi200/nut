#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
mkdir -p "$root/bin" "$root/home" "$root/man"
install -m 0755 "$1" "$root/bin/nut"
shopt -s nullglob
man_pages=("$repo"/man/*.1)
if [[ ${#man_pages[@]} -eq 0 ]]; then
    echo "install-smoke: missing man pages; run just docs-gen" >&2
    exit 1
fi
install -m 0644 "${man_pages[@]}" "$root/man/"
test -s "$root/man/nut.1"
test -s "$root/man/nut-status.1"
test -s "$root/man/nut-doctor.1"
NUT_SMOKE_BINARY="$root/bin/nut" cargo test --locked --test completion -- --exact install_smoke_all_visible_help
NUT_SMOKE_BINARY="$root/bin/nut" cargo test --locked --test docs_gen -- --exact install_smoke_every_command
export HOME="$root/home" XDG_STATE_HOME="$root/state" XDG_DATA_HOME="$root/data"
export PATH="$root/bin:$PATH"
unset ZDOTDIR BASH_ENV ENV BASH_COMPLETION_USER_DIR
nut --version
config="$repo/examples/host.toml"
nut --config "$config" config path
nut --config "$config" config show >/dev/null
nut doctor --list >/dev/null
nut bios profile list >/dev/null
nut completion zsh >/dev/null
nut completion bash >/dev/null
nut drill >/dev/null
for shell in zsh bash; do
    nut completion install "$shell" --target "$root/$shell"
done
nut completion status --json
nut init --dry-run --help >/dev/null
nut rollback --list --help >/dev/null
nut rollback --list >/dev/null
nut init --dry-run 2>/dev/null || true
nut rollback --dry-run 2>/dev/null || true
nut completion uninstall
test ! -e "$root/zsh/_nut"
test ! -e "$root/bash/nut"
