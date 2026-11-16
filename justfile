set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

rule := "────────────────────────────────────────────────"
rustc_gate := "-D unused -A unused_extern_crates -A unused_macro_rules -D private_interfaces"
clippy_gate := (
    "-D clippy::bind_instead_of_map "
    + "-D clippy::bool_assert_comparison "
    + "-D clippy::bool_comparison "
    + "-D clippy::chars_next_cmp "
    + "-D clippy::clone_on_copy "
    + "-D clippy::cmp_owned "
    + "-D clippy::collapsible_else_if "
    + "-D clippy::collapsible_if "
    + "-D clippy::collapsible_match "
    + "-D clippy::collapsible_str_replace "
    + "-D clippy::comparison_to_empty "
    + "-D clippy::derivable_impls "
    + "-D clippy::double_comparisons "
    + "-D clippy::explicit_auto_deref "
    + "-D clippy::extend_with_drain "
    + "-D clippy::filter_map_bool_then "
    + "-D clippy::for_kv_map "
    + "-D clippy::items_after_test_module "
    + "-D clippy::iter_cloned_collect "
    + "-D clippy::len_zero "
    + "-D clippy::let_and_return "
    + "-D clippy::manual_contains "
    + "-D clippy::manual_ignore_case_cmp "
    + "-D clippy::manual_is_multiple_of "
    + "-D clippy::manual_pattern_char_comparison "
    + "-D clippy::manual_range_patterns "
    + "-D clippy::manual_rem_euclid "
    + "-D clippy::manual_unwrap_or "
    + "-D clippy::map_identity "
    + "-D clippy::needless_as_bytes "
    + "-D clippy::needless_borrow "
    + "-D clippy::needless_borrows_for_generic_args "
    + "-D clippy::needless_option_as_deref "
    + "-D clippy::needless_return "
    + "-D clippy::op_ref "
    + "-D clippy::print_literal "
    + "-D clippy::question_mark "
    + "-D clippy::redundant_closure "
    + "-D clippy::single_char_add_str "
    + "-D clippy::single_element_loop "
    + "-D clippy::single_match "
    + "-D clippy::unnecessary_lazy_evaluations "
    + "-D clippy::unnecessary_map_or "
    + "-D clippy::unnecessary_to_owned "
    + "-D clippy::useless_concat "
    + "-D clippy::useless_conversion "
    + "-D clippy::useless_format "
    + "-D clippy::while_let_on_iterator"
)

# List available development recipes.
default:
    @just --list

# Run all local checks.
all: check

# Check Rust formatting without changing files.
fmt: (_banner "36" "FORMAT")
    @cargo metadata --locked --no-deps --format-version 1 >/dev/null
    cargo fmt --all -- --check

# Apply Rust formatting.
format:
    @cargo metadata --locked --no-deps --format-version 1 >/dev/null
    cargo fmt --all

# Check every target with the compiler and Clippy gates.
lint: (_banner "33" "LINT")
    cargo clippy --locked --workspace --all-targets --all-features -- {{rustc_gate}} {{clippy_gate}} -D clippy::disallowed_methods

# Run every test binary and doctest, continuing after failures.
test: (_banner "32" "TEST")
    cargo test --locked --workspace --all-features --no-fail-fast

# Shellcheck the golden rendered event hook when shellcheck is installed.
hook-shellcheck: (_banner "35" "HOOK")
    @if command -v shellcheck >/dev/null; then \
        shellcheck -s sh tests/fixtures/hosts/athena/expected/nut-upssched-cmd; \
    else \
        echo "shellcheck not installed; skipping hook check"; \
    fi

# Build, install into a temporary root, smoke all help, and exercise completion.
install-smoke:
    cargo build --locked --bin nut
    bash scripts/install_smoke.sh "${CARGO_TARGET_DIR:-target}/debug/nut"

# Review insta snapshots.
snap:
    cargo insta review

# Run the canonical offline formatting, lint, test, and hook shellcheck gate.
check: fmt lint test hook-shellcheck

# Run nut with the given arguments.
run *args:
    cargo run --locked -- {{args}}

# Build a release binary, copy it to HOST, and run a read-only nut command over SSH.
remote host +args:
    #!/usr/bin/env bash
    set -euo pipefail
    args=({{args}})
    scan=("${args[@]}")
    while [[ ${#scan[@]} -gt 0 && "${scan[0]}" == -* ]]; do
        case "${scan[0]}" in
            --color|--config|-c|-s|-u)
                if [[ ${#scan[@]} -lt 2 ]]; then
                    echo "just remote: ${scan[0]} needs a value" >&2
                    exit 2
                fi
                scan=("${scan[@]:2}")
                ;;
            *)
                scan=("${scan[@]:1}")
                ;;
        esac
    done
    subcommand="${scan[0]:-}"
    action="${scan[1]:-}"
    extra="${scan[2]:-}"
    allowed=0
    case "$subcommand" in
        ""|-h|--help|-V|--version) allowed=1 ;;
        status|battery|events|doctor) allowed=1 ;;
        config)
            case "$action" in
                ""|show|path|diff|-h|--help) allowed=1 ;;
            esac
            ;;
        bios)
            case "$action" in
                ""|guide|profile|-h|--help) allowed=1 ;;
            esac
            ;;
        beeper|drill)
            [[ -z "$action" || "$action" == "-h" || "$action" == "--help" ]] && allowed=1
            ;;
        init)
            for arg in "${scan[@]}"; do
                if [[ "$arg" == "--dry-run" || "$arg" == "-n" ]]; then
                    allowed=1
                fi
            done
            ;;
        completion)
            [[ "$action" == "status" || "$action" == "-h" || "$action" == "--help" || -z "$action" ]] && allowed=1
            ;;
    esac
    if [[ "$subcommand" == "bios" && "$action" == "profile" ]]; then
        case "$extra" in
            ""|list|show|new|-h|--help) allowed=1 ;;
            *) allowed=0 ;;
        esac
    fi
    if [[ "$allowed" -ne 1 ]]; then
        echo "just remote: refusing '${args[*]}'; only read-only nut commands are allowed" >&2
        exit 2
    fi
    cargo build --locked --release
    bin="${CARGO_TARGET_DIR:-target}/release/nut"
    ssh -T "{{host}}" mkdir -p ~/.cache/nut-cli
    scp -q "$bin" "{{host}}:.cache/nut-cli/nut"
    ssh -T "{{host}}" ~/.cache/nut-cli/nut "${args[@]}"

# Run status against the checked-in scripted UPS fixture.
demo:
    cargo build --locked --bins
    @set -eu; \
      setsid cargo run --quiet --locked --bin nut-fake-ups-d -- --listen 127.0.0.1:43493 --fixture examples/fixtures/demo-ups.json & fake=$!; \
      trap 'kill -TERM -- "-$fake" >/dev/null 2>&1 || true; wait "$fake" >/dev/null 2>&1 || true' EXIT; \
      for _ in {1..50}; do \
        if cargo run --quiet --locked --bin nut -- --color never --config examples/demo.toml status --upsd-port 43493 >/dev/null 2>&1; then break; fi; \
        sleep 0.1; \
      done; \
      cargo run --quiet --locked --bin nut -- --config examples/demo.toml status --upsd-port 43493

# Serve the demo fixture on a loopback-only fake upsd.
fake-upsd fixture="examples/fixtures/demo-ups.json" listen="127.0.0.1:43493":
    cargo run --locked --bin nut-fake-ups-d -- --listen "{{listen}}" --fixture "{{fixture}}"

# Read status through an SSH tunnel. The host argument is required.
remote-status host ups="cp1500":
    bash scripts/remote_ups.sh "{{host}}" status "{{ups}}"

# Read battery data through an SSH tunnel. The host argument is required.
remote-battery host ups="cp1500":
    bash scripts/remote_ups.sh "{{host}}" battery "{{ups}}"

# Capture the selected, redacted status fields into a local JSON fixture.
fixture host output ups="cp1500":
    bash scripts/remote_ups.sh "{{host}}" fixture "{{ups}}" "{{output}}"

# Render a section banner, using color only on a terminal.
_banner color label:
    @if [[ -t 1 && -z "${NO_COLOR:-}" && "${TERM:-}" != dumb ]]; then \
        printf '\n  \033[1;%sm%s\033[0m\n\033[%sm%s\033[0m\n' '{{color}}' '{{label}}' '{{color}}' '{{rule}}'; \
    else \
        printf '\n  %s\n%s\n' '{{label}}' '{{rule}}'; \
    fi
