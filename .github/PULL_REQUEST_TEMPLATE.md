## What and why

<!-- What changes, and what problem it solves. Link the issue if there is one. -->

## Checklist

- [ ] Tests cover the happy path, the error paths, and any limit boundaries touched
- [ ] `cargo fmt --all` / `cargo clippy --all-targets -- -D warnings` / `cargo test --workspace`
- [ ] `./scripts/check-core-purity.sh` passes
- [ ] Public API changes considered for backward compatibility
- [ ] Documentation updated (rustdoc, and `docs/` if behaviour or design changed)

## Storage format

- [ ] This PR does **not** change the on-disk format

<!-- If it does, delete the line above, write `FORMAT-CHANGE:` and a rationale below, bump
     `format_version`, and describe the migration path. Golden fixtures in testdata/ may only
     change with an explanation here. -->

## Performance claims

<!-- If you are claiming a speedup, link the benchmark run. No numbers without a benchmark. -->
