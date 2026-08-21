#!/usr/bin/env bash
# Enforces the central architectural rule: the core engine performs no I/O.
#
# docs/architecture/03-system-design.md says the core knows nothing about the platform it runs
# on. A rule that is not checked by CI is a rule that gets broken by month three, so this script
# checks it. It runs in CI on every push and is fast enough to run locally before a commit.
set -euo pipefail

cd "$(dirname "$0")/.."

PURE_CRATES=(vdb-core vdb-format)

# Modules that mean "this code talks to the outside world". `std::collections`, `std::sync` and
# the like are fine — they are data structures, not I/O.
declare -a FORBIDDEN=(
  'std::fs'
  'std::net'
  'std::thread'
  'std::process'
  'std::env'
  'std::os::'
  'SystemTime'
  'Instant::now'
  'std::io::stdin'
  'std::io::stdout'
  'std::io::stderr'
  'println!'
  'eprintln!'
)

fail=0

for crate in "${PURE_CRATES[@]}"; do
  dir="crates/$crate/src"
  [[ -d "$dir" ]] || continue

  for pattern in "${FORBIDDEN[@]}"; do
    # Exclude doc comments and ordinary comments: naming a forbidden thing while explaining why
    # it is forbidden must not fail the check.
    if hits=$(grep -rn --include='*.rs' -F "$pattern" "$dir" \
                | grep -v -E '^\s*[^:]+:[0-9]+:\s*(///|//!|//|\*)' || true); [[ -n "$hits" ]]; then
      echo "FAIL: $crate uses '$pattern' — the core must not perform I/O."
      echo "$hits" | sed 's/^/       /'
      fail=1
    fi
  done

  # unsafe is forbidden outright in these crates: they parse untrusted bytes.
  if ! grep -q '#!\[forbid(unsafe_code)\]' "crates/$crate/src/lib.rs"; then
    echo "FAIL: crates/$crate/src/lib.rs is missing #![forbid(unsafe_code)]."
    fail=1
  fi

  # Third-party dependencies here need an ADR. Workspace crates (vdb-*) are fine: the layering
  # is what this script is protecting, not the dependency count.
  deps=$(awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f && NF && $0 !~ /^#/' \
           "crates/$crate/Cargo.toml" | grep -v '^vdb-' || true)
  if [[ -n "$deps" ]]; then
    echo "FAIL: $crate has third-party dependencies; each needs an ADR (docs/adr/README.md)."
    echo "$deps" | sed 's/^/       /'
    fail=1
  fi
done

if [[ $fail -eq 0 ]]; then
  echo "core purity: OK (${PURE_CRATES[*]} perform no I/O, forbid unsafe, and pull in no third-party crates)"
fi
exit $fail
