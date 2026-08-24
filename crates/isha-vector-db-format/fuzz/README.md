# Format fuzzing

Every decoder in `isha-vector-db-format` reads bytes that may be corrupt, truncated, or hostile. These
targets assert the properties that matter for a database people trust with their data:

1. **No decoder panics, aborts, hangs, or exhausts memory on any input.** A crash here is a
   crash inside the host application, and for a library embedded in someone's phone app that is
   the worst failure mode available to us.
2. **Anything that decodes must re-encode to exactly the same bytes.** The format is canonical;
   if a decoder accepts an encoding its own encoder would never produce, then two different byte
   sequences mean the same thing, and checksums, golden fixtures and compaction verification all
   become unreliable.

   One exception, and it is deliberate: a v1 file writes a map of eight or more fields under the
   plain tag, and a v2 build must still read it ([ADR-0014](../../../docs/adr/0014-metadata-offset-table.md)).
   Re-encoding upgrades such a record to the indexed form, so its bytes get strictly longer.
   `value` allows exactly that divergence and nothing else.

## Running

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run value -- -max_total_time=300
cargo +nightly fuzz run wal   -- -max_total_time=300
```

CI runs each target for an hour nightly. Any crash found is committed to
`fuzz/corpus/<target>/` as a regression case *and* added to the unit tests, so a fix cannot
silently regress once the fuzzer moves on.
