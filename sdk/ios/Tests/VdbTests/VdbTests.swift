import Foundation
import XCTest

@testable import Vdb

/// The Swift API, driven on macOS.
///
/// Running here rather than only in a simulator is what makes this loop seconds instead of
/// minutes. The parts that genuinely need a device — Data Protection while the screen is locked,
/// the app sandbox, jetsam behaviour under a large scan — belong in a separate suite that runs
/// far less often, and none of them are about whether this API works.
final class VdbTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("vdb-swift-\(UUID().uuidString)")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: directory)
    }

    private func open(readOnly: Bool = false) throws -> Database {
        try Database(path: directory.path, readOnly: readOnly)
    }

    // ---- lifecycle ----

    func testVersionsAreReported() {
        XCTAssertFalse(Version.library.isEmpty)
        XCTAssertEqual(Version.abi, 1)
        XCTAssertEqual(Version.format, 1)
    }

    func testWriteSearchAndRead() throws {
        let db = try open()
        let docs = try db.collection("docs", dimension: 3)

        XCTAssertTrue(try docs.upsert("east", vector: [1, 0, 0]), "a new document")
        XCTAssertFalse(try docs.upsert("east", vector: [1, 0, 0]), "a replacement")
        try docs.upsert("north", vector: [0, 1, 0])
        XCTAssertEqual(try docs.count(), 2)
        XCTAssertTrue(try docs.contains("east"))

        let hits = try docs.search([0.9, 0.1, 0], topK: 2)
        XCTAssertEqual(hits.count, 2)
        XCTAssertEqual(hits[0].id, "east")
        XCTAssertGreaterThan(hits[0].score, hits[1].score, "ordered by score descending")
        XCTAssertEqual(hits[0].score, 1.0, accuracy: 0.02)

        XCTAssertTrue(try docs.delete("east"))
        XCTAssertFalse(try docs.delete("east"), "deleting twice is a no-op")
        XCTAssertEqual(try docs.count(), 1)
        try db.close()
    }

    func testSearchOnAnEmptyCollectionReturnsNothing() throws {
        let db = try open()
        let docs = try db.collection("docs", dimension: 2)
        XCTAssertTrue(try docs.search([1, 0], topK: 10).isEmpty)
        try db.close()
    }

    func testEveryMetricWorks() throws {
        for metric in [Metric.cosine, .l2, .dot] {
            let dir = FileManager.default.temporaryDirectory
                .appendingPathComponent("vdb-metric-\(UUID().uuidString)")
            defer { try? FileManager.default.removeItem(at: dir) }

            let db = try Database(path: dir.path)
            let docs = try db.collection("docs", dimension: 2, metric: metric)
            try docs.upsert("near", vector: [1, 0])
            try docs.upsert("far", vector: [-1, 0])
            let hits = try docs.search([1, 0], topK: 1)
            XCTAssertEqual(hits.first?.id, "near", "\(metric)")
            try db.close()
        }
    }

    func testDataSurvivesCloseAndReopen() throws {
        do {
            let db = try open()
            let docs = try db.collection("docs", dimension: 2)
            try docs.upsert("kept", vector: [1, 0])
            try db.close()
        }
        let db = try open()
        let docs = try db.openCollection("docs")
        XCTAssertTrue(try docs.contains("kept"))
        XCTAssertEqual(try docs.count(), 1)
        try db.close()
    }

    func testCollectionsCanBeDropped() throws {
        let db = try open()
        _ = try db.collection("doomed", dimension: 2)
        _ = try db.collection("kept", dimension: 2)
        try db.dropCollection("doomed")
        XCTAssertThrowsError(try db.openCollection("doomed"))
        _ = try db.openCollection("kept")
        try db.close()
    }

    // ---- errors ----

    func testErrorsCarryTheirStableCode() throws {
        let db = try open()
        let docs = try db.collection("docs", dimension: 3)

        XCTAssertThrowsError(try docs.upsert("a", vector: [1, 0])) { error in
            guard let e = error as? VdbError else { return XCTFail("wrong error type") }
            XCTAssertEqual(e.code, 4003, "the stable code should be matchable")
            XCTAssertTrue(e.message.contains("docs"), e.message)
            XCTAssertTrue(e.message.contains("3-dimensional"), e.message)
        }
        // A rejected write leaves nothing behind.
        XCTAssertEqual(try docs.count(), 0)
        try db.close()
    }

    func testMisuseThrowsRatherThanCrashing() throws {
        let db = try open()
        let docs = try db.collection("docs", dimension: 3)
        XCTAssertThrowsError(try docs.search([1, 0], topK: 1), "wrong dimension")
        XCTAssertThrowsError(try docs.search([1, 0, 0], topK: 0), "zero topK")
        XCTAssertThrowsError(try db.openCollection("nope"), "unknown collection")
        try db.close()
    }

    func testUsingAClosedDatabaseThrows() throws {
        let db = try open()
        try db.close()
        XCTAssertFalse(db.isOpen)
        XCTAssertNoThrow(try db.close(), "closing twice is idempotent")
        XCTAssertThrowsError(try db.collection("docs", dimension: 2))
    }

    // ---- locking ----

    func testASecondWriterIsRefused() throws {
        let first = try open()
        XCTAssertThrowsError(try open()) { error in
            let message = (error as? VdbError)?.message ?? ""
            XCTAssertTrue(message.lowercased().contains("already open"), message)
        }
        try first.close()
        try open().close()
    }

    func testAReaderCanInspectADatabaseAWriterHolds() throws {
        let writer = try open()
        let docs = try writer.collection("docs", dimension: 2)
        try docs.upsert("a", vector: [1, 0])
        try writer.flush()

        let reader = try open(readOnly: true)
        XCTAssertEqual(try reader.openCollection("docs").count(), 1)
        XCTAssertThrowsError(try reader.collection("other", dimension: 2), "read-only refuses writes")
        try reader.close()
        try writer.close()
    }

    /// A database released without an explicit close must still give up its lock, or an
    /// application that drops a reference on an error path can never reopen its own data.
    func testDeinitReleasesTheLock() throws {
        do {
            let db = try open()
            _ = try db.collection("docs", dimension: 2)
            // No close: the reference simply goes out of scope.
        }
        let again = try open()
        XCTAssertEqual(try again.openCollection("docs").count(), 0)
        try again.close()
    }

    // ---- scale ----

    func testManyDocumentsAcrossSegments() throws {
        let db = try open()
        let docs = try db.collection("docs", dimension: 4)
        for i in 0..<500 {
            let angle = Float(i) * 2 * .pi / 500
            try docs.upsert("doc-\(i)", vector: [cos(angle), sin(angle), 0.25, -0.5])
        }
        try db.flush()
        XCTAssertEqual(try docs.count(), 500)

        // A document in the middle must be findable by its own vector.
        let angle = Float(250) * 2 * .pi / 500
        let hits = try docs.search([cos(angle), sin(angle), 0.25, -0.5], topK: 1)
        XCTAssertEqual(hits.first?.id, "doc-250")
        try db.close()
    }
}

/// Filters, through the Swift tree and the ABI's postfix builder beneath it.
final class FilterTests: XCTestCase {
    private var directory: URL!
    private var db: Database!
    private var docs: Collection!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("vdb-filter-\(UUID().uuidString)")
        db = try Database(path: directory.path)
        docs = try db.collection("docs", dimension: 2)

        // Decreasing similarity to [1, 0], so a filter changing *which* documents come back is
        // distinguishable from one changing their order.
        try docs.upsert("hammer", vector: [1, 0],
                        metadata: ["category": .string("tools"), "price": .double(25), "sale": .bool(true)])
        try docs.upsert("saw", vector: [0.95, 0.31],
                        metadata: ["category": .string("tools"), "price": .double(75)])
        try docs.upsert("ball", vector: [0.7, 0.7],
                        metadata: ["category": .string("toys")])
    }

    override func tearDownWithError() throws {
        try? db.close()
        try? FileManager.default.removeItem(at: directory)
    }

    private func ids(_ filter: Filter, topK: Int = 10) throws -> [String] {
        try docs.search([1, 0], topK: topK, filter: filter).map(\.id)
    }

    func testASimpleFilterNarrowsWithoutReordering() throws {
        XCTAssertEqual(try docs.search([1, 0], topK: 3).map(\.id), ["hammer", "saw", "ball"])
        XCTAssertEqual(try ids(.equals("category", .string("tools"))), ["hammer", "saw"])
    }

    func testFiltersCompose() throws {
        let cheapTools = Filter.equals("category", .string("tools"))
            && .lessThan("price", .double(50))
        XCTAssertEqual(try ids(cheapTools), ["hammer"])

        let either = Filter.equals("category", .string("toys"))
            || .greaterThan("price", .double(50))
        XCTAssertEqual(try ids(either), ["saw", "ball"])

        XCTAssertEqual(try ids(!Filter.equals("category", .string("tools"))), ["ball"])
    }

    func testDeepNesting() throws {
        // (tools AND (cheap OR on sale)) OR toys — three levels, one expression.
        let filter = Filter.any([
            .all([
                .equals("category", .string("tools")),
                .any([.lessThan("price", .double(50)), .equals("sale", .bool(true))]),
            ]),
            .equals("category", .string("toys")),
        ])
        XCTAssertEqual(try ids(filter), ["hammer", "ball"])
    }

    func testAbsentFieldsBehaveAsDocumented() throws {
        // "ball" has no price.
        XCTAssertEqual(try ids(.exists("price")), ["hammer", "saw"])
        XCTAssertEqual(try ids(.isNull("price")), ["ball"])
        // notEquals is the exact negation of equals, so it matches the absent field too.
        XCTAssertEqual(try ids(.notEquals("price", .double(25))), ["saw", "ball"])
    }

    func testPrefixAndArrayMembership() throws {
        XCTAssertEqual(try ids(.startsWith("category", "too")), ["hammer", "saw"])
        // Not a substring test: "tools" contains "too" as text but is not an array.
        XCTAssertEqual(try ids(.contains("category", .string("too"))), [])
    }

    func testEmptyCombinatorsAreTheIdentityOfTheirOperation() throws {
        XCTAssertEqual(try ids(.all([])), ["hammer", "saw", "ball"], "an empty all matches everything")
        XCTAssertEqual(try ids(.any([])), [], "an empty any matches nothing")
    }

    func testTopKCountsMatchesNotCandidates() throws {
        // Only one of the three matches, and asking for two returns the one rather than
        // silently returning fewer because the nearest were excluded.
        XCTAssertEqual(try ids(.equals("category", .string("toys")), topK: 2), ["ball"])
    }

    func testAFilterMatchingNothingReturnsNothing() throws {
        XCTAssertEqual(try ids(.equals("category", .string("nonexistent"))), [])
    }

    func testFiltersWorkAfterAFlush() throws {
        try db.flush()
        XCTAssertEqual(try ids(.equals("category", .string("tools"))), ["hammer", "saw"])
    }

    /// A type mismatch is false, never an error — the property the whole filter design rests on.
    func testTypeMismatchesAreFalseRatherThanErrors() throws {
        XCTAssertEqual(try ids(.equals("category", .int(1))), [])
        XCTAssertEqual(try ids(.greaterThan("category", .int(1))), [])
        // And gt/lte are both false there, so they are not negations of each other.
        XCTAssertEqual(try ids(.lessThanOrEqual("category", .int(1))), [])
    }
}

/// Stats, compaction and verification — the tools an application needs to manage its own
/// storage. On a phone this matters more than anywhere else: space is scarce, and until now an
/// app had no way to reclaim what deletes had left behind.
final class MaintenanceTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("vdb-maint-\(UUID().uuidString)")
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: directory)
    }

    func testStatsCompactionAndVerification() throws {
        let db = try Database(path: directory.path)
        let docs = try db.collection("docs", dimension: 2)
        for i in 0..<10 {
            try docs.upsert("doc-\(i)", vector: [Float(i), 1])
        }
        // Flush first: a delete only occupies space once the row it shadows is on disk.
        try docs.flush()
        for i in 0..<7 {
            try docs.delete("doc-\(i)")
        }
        try docs.flush()

        let before = try docs.stats()
        XCTAssertEqual(before.liveDocuments, 3)
        XCTAssertEqual(before.totalRows, 10, "the dead rows are still there")
        XCTAssertEqual(before.dimension, 2)
        XCTAssertGreaterThan(before.deadRatio, 0.6)

        let clean = try db.verify(.full)
        XCTAssertTrue(clean.isClean)
        XCTAssertGreaterThan(clean.warnings, 0, "seventy percent dead is worth a warning")

        XCTAssertEqual(try db.compact(), 7)

        let after = try docs.stats()
        XCTAssertEqual(after.liveDocuments, 3, "compaction must not lose a document")
        XCTAssertEqual(after.totalRows, 3, "the dead rows should be gone")
        XCTAssertEqual(after.deadRatio, 0)
        XCTAssertTrue(try db.verify(.full).isClean)

        // Still searchable afterwards.
        XCTAssertEqual(try docs.search([9, 1], topK: 1).first?.id, "doc-9")
        try db.close()
    }

    func testCompactionLeavesHealthySegmentsAlone() throws {
        let db = try Database(path: directory.path)
        let docs = try db.collection("docs", dimension: 2)
        for i in 0..<10 {
            try docs.upsert("doc-\(i)", vector: [Float(i), 1])
        }
        try docs.flush()
        try docs.delete("doc-0")
        try docs.flush()

        XCTAssertEqual(try db.compact(), 0, "ten percent dead is not worth rewriting")
        XCTAssertEqual(try db.compact(minDeadRatio: 0), 1, "unless asked to rewrite everything")
        try db.close()
    }

    func testANonsensicalRatioIsRefusedRatherThanClamped() throws {
        let db = try Database(path: directory.path)
        XCTAssertThrowsError(try db.compact(minDeadRatio: 2))
        XCTAssertThrowsError(try db.compact(minDeadRatio: -1))
        try db.close()
    }
}
