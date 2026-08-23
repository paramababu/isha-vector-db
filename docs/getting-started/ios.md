# isha-vector-db on iOS

Swift, through a C interop layer. There is no Objective-C in the middle.

## Install

Swift Package Manager, in Xcode: **File → Add Package Dependencies**, then

```text
https://github.com/paramababu/isha-vector-db
```

or in a `Package.swift`:

```swift
dependencies: [
    .package(url: "https://github.com/paramababu/isha-vector-db", from: "0.0.1"),
],
targets: [
    .target(name: "MyApp", dependencies: [.product(name: "IshaVectorDB", package: "isha-vector-db")]),
]
```

> **The package is not tagged for release yet.** Build from a checkout for now — see below,
> and [why](README.md#not-yet-published).

iOS 13.4 or newer. The package carries a prebuilt `XCFramework`, so there is nothing to compile
and no bridging header.

### From a checkout

```bash
./scripts/build-xcframework.sh   # device + simulator slices
./scripts/test-swift.sh
```

## Where to put the database

Application Support, not Documents — a search index is derived data the user did not create, and
Documents is visible in the Files app.

```swift
let root = try FileManager.default.url(
    for: .applicationSupportDirectory, in: .userDomainMask,
    appropriateFor: nil, create: true
)
let path = root.appendingPathComponent("notes").path
```

Exclude it from iCloud backup unless you want it uploaded:

```swift
var url = root.appendingPathComponent("notes")
var values = URLResourceValues()
values.isExcludedFromBackup = true
try url.setResourceValues(&values)
```

## Your first database

```swift
import IshaVectorDB

let db = try Database.open(at: path)
defer { db.close() }

let notes = try db.collection("notes", dimension: 4)

try notes.upsert("note-1", vector: [1, 0, 0, 0])
try notes.upsert("note-2", vector: [0.9, 0.1, 0, 0])
try notes.upsert("note-3", vector: [0, 0, 1, 0])
try notes.flush()

for hit in try notes.search([1, 0, 0, 0], topK: 2) {
    print(hit.id, hit.score)
}
```

`defer { db.close() }` immediately after opening is the pattern to keep. An open database holds a
lock, and a `throw` that skips the close leaves it held.

## Never on the main actor

A search blocks for its duration, and blocking the main actor drops frames.

```swift
actor NoteSearch {
    private let db: Database
    private let notes: Collection

    init(path: String) throws {
        db = try Database.open(at: path)
        notes = try db.collection("notes", dimension: 384)
    }

    func search(_ query: [Float], topK: Int = 10) throws -> [Hit] {
        try notes.search(query, topK: topK)
    }

    deinit { db.close() }
}
```

An `actor` gives you serialised access off the main thread, which matches the engine's own
single-writer model.

## Metadata and filters

```swift
try notes.upsert("note-1", vector: embedding, metadata: [
    "kind": .string("meeting"),
    "year": .int(2026),
    "starred": .bool(true),
])

let hits = try notes.search(query, topK: 10, filter: .and([
    .equals("kind", .string("meeting")),
    .greaterThanOrEqual("year", .int(2026)),
]))
```

A filter narrows results, not work. See [filters.md](../api/filters.md).

## Errors

Everything throwing throws `VdbError`, which carries the engine's code.

```swift
do {
    try notes.upsert("bad", vector: [1, 2])
} catch let error as VdbError {
    print(error.code)      // 4003
    print(error.message)   // [VDB-4003] collection "notes" stores 4-dimensional vectors, got 2
}
```

Branch on `code`. [The full list](../api/error-codes.md) is banded, so an unrecognised `5xxx` is
still identifiable as storage trouble.

## Things that catch people out

**`[Float]`, not `[Double]`.** Swift infers `Double` from a bare literal array, so
`let v = [1.0, 2.0]` is `[Double]` and will not compile against these signatures. Annotate it:
`let v: [Float] = [1.0, 2.0]`.

**One handle per database.** A second `Database.open` on the same path throws `VDB-2001`.

**Background suspension.** iOS can suspend your app between a write and a flush. Call `flush()`
at a natural boundary — `scenePhase` moving to `.background` is the obvious one — rather than
relying on the app staying alive.

**Size.** The linked engine adds roughly 662 KB to an app binary after dead-stripping. The
figure without `-dead_strip` is 1.63 MB, which is what you will see if you measure the static
library rather than a linked app.
