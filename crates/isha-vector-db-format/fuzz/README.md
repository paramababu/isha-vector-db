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

## Running

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run value -- -max_total_time=300
cargo +nightly fuzz run wal   -- -max_total_time=300
```

CI runs each target for an hour nightly. Any crash found is committed to
`fuzz/corpus/<target>/` as a regression case *and* added to the unit tests, so a fix cannot
silently regress once the fuzzer moves on.
