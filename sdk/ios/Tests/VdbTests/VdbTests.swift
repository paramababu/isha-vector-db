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
