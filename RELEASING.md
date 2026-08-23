# Releasing

Publishing is a tag push. Everything is built by `.github/workflows/release.yml` on the platform
it targets, because a binary built on a maintainer's laptop is unreproducible and linked against
whatever that laptop happened to have.

## Before the first release: accounts and secrets

None of this can be done from a repository. Each needs an account you own, and the token from it
in **Settings → Secrets and variables → Actions**.

| Registry | Secret | How to get it |
|---|---|---|
| crates.io | `CARGO_REGISTRY_TOKEN` | `cargo login` → [crates.io/settings/tokens](https://crates.io/settings/tokens), scoped to publish-new and publish-update |
| npm | `NPM_TOKEN` | An **automation** token — a normal one fails under 2FA in CI |
| PyPI | *(none)* | Configure [trusted publishing](https://docs.pypi.org/trusted-publishers/) instead: owner `paramababu`, repo `isha-vector-db`, workflow `release.yml`. No long-lived token in the repository. |

Two more registries need work that does not exist yet:

- **Maven Central** — a Sonatype account, a verified `dev.isha` namespace, and a GPG key whose
  public half is on a keyserver. Every artefact must be signed. The Android AAR is not yet built
  by CI at all.
- **CocoaPods** — `pod trunk register`, and a published `XCFramework`. The podspec in
  `sdk/react-native/` has never been through `pod install`.

**pub.dev is not applicable.** There is no Dart package; the Flutter binding was never built.

## Names

Registry names are claimed on first publish and are effectively permanent. Check they are free,
and claim them before someone else does:

```bash
npm view @isha-vector-db/node          # should 404
pip index versions isha-vector-db      # should find nothing
cargo search isha-vector-db-core
```

An npm **scope** must be created before anything can be published into it, and a scoped package
needs `--access public` on first publish or npm assumes private and rejects it.

## The version number

One version across the workspace, in the root `Cargo.toml`. The release workflow refuses to run
if the tag does not match it, because publishing `v0.2.0` from a tree that says `0.1.0` produces
a release nobody can reproduce from the tag.

`0.1.0` says "usable, and the API may still move", which is accurate. It is deliberately not
`1.0.0`: that would promise the API and the storage format are settled, and neither is.

Four things are versioned separately and deliberately (see the changelog's header): the library,
the **storage format** (an integer, currently 2), the **C ABI** (an integer, frozen at 1), and the
SDK packages. Only the library version goes in the tag.

## Releasing

```bash
# 1. Everything green locally.
cargo test --workspace && ./scripts/check-core-purity.sh && ./scripts/check-references.sh
./scripts/test-python.sh && ./scripts/test-react-native.sh
(cd sdk/node && npm test) && (cd sdk/web && npm test)

# 2. Set the version everywhere it appears.
#    Root Cargo.toml, sdk/*/package.json, sdk/node/npm/*/package.json, sdk/python/pyproject.toml.

# 3. Write the changelog entry. Move [Unreleased] to the new version with a date.

# 4. Dry run: builds every artefact, publishes nothing.
gh workflow run release.yml -f dry_run=true

# 5. When that is green:
git tag -a v0.1.0 -m "0.1.0" && git push origin v0.1.0
```

## What a release actually publishes

| Package | Contents |
|---|---|
| `isha-vector-db-*` (crates.io) | Source. Published in dependency order, because a crate cannot resolve one that is not there yet. |
| `isha-vector-db` (PyPI) | One wheel per platform, each carrying that platform's shared library, tagged `py3-none-<platform>`. |
| `@isha-vector-db/node` | JavaScript only. The addon comes from a platform package. |
| `@isha-vector-db/node-<platform>` | One native addon each, with `os` and `cpu` set so npm downloads only the matching one. |
| `@isha-vector-db/web` | The WebAssembly module. One artefact for every platform, because wasm has none. |
| `@isha-vector-db/react-native` | Sources. The native side is built by the consuming app. |

## Things that will go wrong the first time

**The npm scope does not exist.** Create it at npmjs.com before the first publish, or every
package fails with a 404 that reads like a network error.

**`@isha-vector-db/node` is published before its platform packages.** npm resolves optional
dependencies at install time, so the main package must go last. The workflow does this; a manual
publish is where it gets forgotten.

**A version is already taken.** PyPI and npm both refuse a re-upload of the same version, even
after a delete. A failed release burns that number — go to the next patch rather than trying to
reuse it. This is why the manual trigger defaults to a dry run.

**crates.io rate-limits new crates.** Publishing ten at once from a fresh account may hit a cap;
the limit is lifted on request.

**A wheel tagged `any`.** That means `sdk/python/setup.py` did not see a bundled library, so the
wheel would install on any machine and fail at import on most of them. The workflow checks for
this and fails the build.

## After

- `pip install isha-vector-db` in a clean virtualenv, on a machine that is not the build machine.
- `npm install @isha-vector-db/node` on Linux, which is the platform most likely to be missing.
- The GitHub release carries the wheels, the `.wasm`, and `vdb.h`, so a C consumer needs no
  registry at all.
