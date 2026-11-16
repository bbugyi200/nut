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

# Run the canonical offline formatting, lint, and test gate.
check: fmt lint test

# Render a section banner, using color only on a terminal.
_banner color label:
    @if [[ -t 1 && -z "${NO_COLOR:-}" && "${TERM:-}" != dumb ]]; then \
        printf '\n  \033[1;%sm%s\033[0m\n\033[%sm%s\033[0m\n' '{{color}}' '{{label}}' '{{color}}' '{{rule}}'; \
    else \
        printf '\n  %s\n%s\n' '{{label}}' '{{rule}}'; \
    fi
