# 2. Technology Choice for the Core Engine

**Decision: Rust, compiled to a single `staticlib`/`cdylib` per target, exposed through one
hand-written C ABI, plus a `wasm32-unknown-unknown` build for the web.**

## 2.1 Why the core must be a *native compiled* language

The requirement list forces this before we even compare languages:

- The core must run inside React Native, Flutter, Android, iOS, Node.js, desktop and the browser.
  The only artifact all six can consume is a native library with a C ABI (plus a WASM build for
  the browser). Anything else means writing the engine more than once.
- Brute-force search is a memory-bandwidth-bound tight loop over `f32` arrays. It needs SIMD
  intrinsics, control over allocation, and no GC.
- Mobile processes get killed at arbitrary instruction boundaries. Durability requires precise
  control over write ordering and `fsync`. Runtimes that buffer I/O opaquely make this guesswork.
- App-size and RSS budgets on mobile rule out shipping a language runtime.

## 2.2 Candidate comparison

| | Rust | C++17/20 | Zig | Go | TypeScript core | Dart core |
|---|---|---|---|---|---|---|
| C ABI export | native | native | native | cgo, awkward | no | no |
| WASM target | first-class | via Emscripten | good | large binaries, poor | n/a (is JS) | dart2wasm, immature for FFI |
| iOS static lib | yes | yes | yes | painful | no | no |
| Android NDK | `cargo-ndk`, easy | yes | yes | possible | no | no |
| Memory safety | compiler-enforced | manual | manual+ | GC | GC | GC |
| GC pauses | none | none | none | yes | yes | yes |
| Binary size (stripped, this feature set) | ~0.8–1.5 MB | ~0.6–1.2 MB | ~0.5–1 MB | 5–12 MB | n/a | n/a |
| Fuzzing / sanitizers / property testing | cargo-fuzz, miri, proptest, loom | libFuzzer, ASAN | limited | limited | limited | limited |
| Dependency & build reproducibility | cargo | CMake/vcpkg/Conan sprawl | good | good | npm | pub |
| Ecosystem for our needs (mmap, SIMD, napi, wasm-bindgen, UniFFI) | excellent | good but manual | thin | thin | n/a | n/a |
| Contributor pool for an OSS DB | good | good | small | good | largest | small |
| Risk of UB in a *parser of untrusted files* | low | **high** | medium | low | low | low |

### Why Rust wins

1. **One codebase, six targets.** `cargo build --target aarch64-linux-android / aarch64-apple-ios /
   x86_64-pc-windows-msvc / wasm32-unknown-unknown` all work from the same source, in CI, today.
2. **We are writing a parser for untrusted bytes.** A database file may be corrupted, truncated,
   or hostile. In C++ that is the single most common source of exploitable memory-safety bugs.
   In Rust, a corrupt length field is a `Result::Err`, not a heap overflow. This one argument
   would be enough on its own.
3. **The test tooling is the differentiator, not the syntax.** `cargo-fuzz` on the format decoders,
   `proptest` for round-trip invariants, `miri` for the `unsafe` in the mmap/SIMD paths, `loom` for
   the lock protocol. Reliability is the product here; the language with the best correctness
   tooling wins.
4. **Bindings are solved problems.** `napi-rs` (Node), `wasm-bindgen` (Web), `cbindgen` (C header
   for RN/Flutter), `jni` crate (Android), UniFFI (Kotlin/Swift, optional). No exotic glue.
5. **No runtime to ship.** `panic = "abort"`, `lto = "fat"`, `opt-level = "z"` on mobile profiles.

### Why not the others

- **C++** is genuinely competitive on the technical axes and loses on build/dependency management
  across six platforms, and on memory safety in exactly the code that handles untrusted input.
  Choosing C++ here means paying for ASAN/UBSAN/fuzzing infrastructure that Rust gives for free.
- **Zig** would be a fine choice in 3 years. Pre-1.0 language churn on a project whose selling
  point is stability is a bad trade.
- **Go**: cgo makes it a *bad* C-ABI producer, mobile support is second-class, binaries are large,
  and the GC undermines the latency predictability that is half the point of an embedded engine.
- **A TypeScript core** is tempting because 3 of 7 targets are JS. But then Flutter/Android/iOS get
  either nothing or a second implementation, and two implementations of a storage format will
  diverge — that is the failure mode that kills projects like this. Same argument against a Dart
  core (which would strand JS and native).

## 2.3 Language for each layer

| Layer | Language | Reason |
|---|---|---|
| Engine, format, indexes, storage adapters | Rust | above |
| Stable ABI | C (via `cbindgen`-generated `vdb.h`) | the universal interop currency |
| Node.js addon | Rust (`napi-rs`) → npm package | N-API is ABI-stable across Node versions |
| Web | Rust → `wasm-bindgen` + a TS worker shim | only viable browser path |
| React Native | C++ JSI shim + TS | zero-copy `ArrayBuffer`, no bridge serialization |
| Flutter | Dart `ffi` + `ffigen` from `vdb.h` | no platform-channel serialization cost |
| Android | Kotlin + a thin JNI shim (C or Rust `jni` crate) | idiomatic Kotlin API |
| iOS/macOS | Swift wrapper over the C header, XCFramework | idiomatic Swift API |
| CLI / benchmarks | Rust | reuse the engine directly |
| Docs site | Markdown (+ mdBook or Docusaurus later) | keep docs in-repo, reviewed with code |

## 2.4 Rust ground rules for this project

- **MSRV** pinned (start at a release ~6 months old) and tested in CI; bumping MSRV is a minor-version event.
- `#![forbid(unsafe_code)]` in `vdb-core` and `vdb-format`. `unsafe` is permitted only in
  `vdb-index-flat` (SIMD), `vdb-storage-os` (mmap) and the binding crates, each block carrying a
  `// SAFETY:` comment, and each of those crates runs under miri/ASAN in CI where applicable.
- **No `panic!` reaches an FFI boundary.** Every exported function wraps its body in
  `catch_unwind` and converts a panic into `VDB_ERR_INTERNAL`. A panic crossing an FFI boundary is
  undefined behaviour and would take the host app down.
- **No `unwrap()`/`expect()` outside tests** — enforced by clippy lint at deny level.
- `vdb-core` compiles with `--no-default-features` and no `std::fs`, `std::net`, `std::time`, or
  `std::thread` usage. Enforced by a CI grep + a `cargo deny`-style import check. This is the
  mechanical guarantee behind "the core is platform-independent"; a rule that isn't checked by CI
  is a rule that will be broken by month three.
