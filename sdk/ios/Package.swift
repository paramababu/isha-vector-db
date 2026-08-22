// swift-tools-version: 5.9
import PackageDescription

// Two ways to link the engine, and the difference matters.
//
// For local development and `swift test`, the static library built by `cargo build --release`
// is linked with `unsafeFlags`. SwiftPM forbids `unsafeFlags` in a package that others depend
// on, which is exactly right: a released package must not point at a path on someone's disk.
//
// The released package replaces this target with a `binaryTarget` naming an `.xcframework`
// built by `scripts/build-xcframework.sh` — device, simulator and macOS slices in one artifact,
// with no build-time dependency on Rust at all.
let repoRoot = "../.."

let package = Package(
    name: "Vdb",
    platforms: [.iOS(.v14), .macOS(.v12)],
    products: [
        .library(name: "Vdb", targets: ["Vdb"])
    ],
    targets: [
        .target(name: "CVdb"),
        .target(
            name: "Vdb",
            dependencies: ["CVdb"],
            linkerSettings: [
                .unsafeFlags([
                    "-L\(repoRoot)/target/release",
                    "-lvdb_ffi",
                ])
            ]
        ),
        .testTarget(name: "VdbTests", dependencies: ["Vdb"]),
    ]
)
