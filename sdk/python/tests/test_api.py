"""The Python binding, against the real engine.

Run with ``python3 -m unittest discover sdk/python/tests`` or via
``scripts/test-python.sh``. Nothing is mocked: every test opens a real database on disk.

The declarations in ``_ffi.py`` are what these mostly protect. A wrong ``argtypes`` does not fail
loudly — ctypes truncates a pointer to 32 bits and the process segfaults at some unrelated later
moment — so the value of this suite is that it exercises every declared signature.
"""

from __future__ import annotations

import array
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import isha_vector_db as vdb  # noqa: E402


class Scratch(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.mkdtemp(prefix="vdb-py-")
        # Registered first so it runs last: cleanups are LIFO, and removing the directory before
        # the database is closed makes close fail looking for its own write-ahead log.
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def open(self, **kwargs) -> vdb.Database:
        db = vdb.open(self.dir, **kwargs)
        self.addCleanup(db.close)
        return db


class TestVersions(unittest.TestCase):
    def test_versions_are_reported(self) -> None:
        v = vdb.version()
        self.assertTrue(v["library"])
        # The ABI is frozen; the on-disk format moves independently of it.
        self.assertEqual(v["abi"], 1)
        self.assertGreaterEqual(v["format"], 1)


class TestLifecycle(Scratch):
    def test_write_search_and_read_back(self) -> None:
        db = self.open()
        docs = db.collection("docs", dimension=4)

        for i in range(8):
            self.assertTrue(docs.upsert(f"doc-{i}", [float(i), 1.0, 0.0, -1.0]))
        self.assertEqual(len(docs), 8)
        self.assertIn("doc-3", docs)
        self.assertNotIn("nope", docs)

        hits = docs.search([7.0, 1.0, 0.0, -1.0], k=3)
        self.assertEqual(len(hits), 3)
        self.assertEqual(hits[0].id, "doc-7")
        # Scores are higher-is-better whatever the metric, so they descend.
        self.assertGreaterEqual(hits[0].score, hits[1].score)

    def test_upserting_the_same_id_replaces(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        self.assertTrue(docs.upsert("a", [1.0, 0.0, 0.0]))
        self.assertFalse(docs.upsert("a", [0.0, 1.0, 0.0]), "the second is not an insert")
        self.assertEqual(len(docs), 1)

    def test_delete(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        docs.upsert("a", [1.0, 0.0, 0.0])
        self.assertTrue(docs.delete("a"))
        self.assertFalse(docs.delete("a"), "deleting an absent document is not an error")
        self.assertEqual(len(docs), 0)

    def test_data_survives_a_reopen(self) -> None:
        db = vdb.open(self.dir)
        docs = db.collection("docs", dimension=4)
        for i in range(5):
            docs.upsert(f"doc-{i}", [float(i), 1.0, 0.0, -1.0])
        docs.flush()
        db.close()

        with vdb.open(self.dir, create_if_missing=False) as reopened:
            again = reopened.collection("docs", dimension=4)
            self.assertEqual(len(again), 5)
            self.assertEqual(again.search([4.0, 1.0, 0.0, -1.0], k=1)[0].id, "doc-4")


class TestVectorTypes(Scratch):
    """Whatever a caller already has should work without converting it first."""

    def test_a_list_of_floats(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        self.assertTrue(docs.upsert("a", [1.0, 2.0, 3.0]))

    def test_a_list_of_ints(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        self.assertTrue(docs.upsert("a", [1, 2, 3]))

    def test_an_array_module_array(self) -> None:
        # array('f') and numpy both expose the buffer protocol, which is the fast path.
        docs = self.open().collection("docs", dimension=3)
        self.assertTrue(docs.upsert("a", array.array("f", [1.0, 2.0, 3.0])))

    def test_a_buffer_and_a_list_agree(self) -> None:
        """The fast path and the slow path must produce the same vector.

        They are different code, and a mistake in the buffer cast would silently store
        reinterpreted bytes rather than fail.
        """
        db = self.open()
        docs = db.collection("docs", dimension=4)
        docs.upsert("list", [1.0, 2.0, 3.0, 4.0])
        docs.upsert("buffer", array.array("f", [1.0, 2.0, 3.0, 4.0]))
        hits = docs.search([1.0, 2.0, 3.0, 4.0], k=2)
        self.assertEqual(len(hits), 2)
        self.assertAlmostEqual(hits[0].score, hits[1].score, places=6)


class TestErrors(Scratch):
    def test_the_engines_message_and_code_survive(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        with self.assertRaises(vdb.VdbError) as caught:
            docs.upsert("bad", [1.0, 2.0])
        # Not a flattened "upsert failed": the structured message has to reach the developer.
        self.assertIn("3-dimensional", str(caught.exception))
        self.assertGreater(caught.exception.code, 0)

    def test_a_bad_specification_reports_the_real_reason(self) -> None:
        db = self.open()
        with self.assertRaises(vdb.VdbError) as caught:
            db.collection("zero", dimension=0)
        # Not "collection not found" from a fallback open, which is what a naive
        # create-then-open would report.
        self.assertNotIn("not found", str(caught.exception))

    def test_a_missing_database_is_an_error(self) -> None:
        with self.assertRaises(vdb.VdbError):
            vdb.open(os.path.join(self.dir, "absent"), create_if_missing=False)

    def test_a_closed_database_refuses_use(self) -> None:
        db = vdb.open(self.dir)
        db.close()
        self.assertFalse(db.is_open)
        with self.assertRaises(vdb.VdbError):
            db.collection("docs", dimension=3)

    def test_closing_twice_is_harmless(self) -> None:
        db = vdb.open(self.dir)
        db.close()
        db.close()

    def test_a_released_collection_refuses_use(self) -> None:
        docs = self.open().collection("docs", dimension=3)
        docs.release()
        with self.assertRaises(vdb.VdbError):
            len(docs)
        docs.release()

    def test_a_collection_is_not_iterable(self) -> None:
        # There is no ordering over a vector index that would mean anything, and silently
        # iterating in storage order would be a trap.
        docs = self.open().collection("docs", dimension=3)
        with self.assertRaises(TypeError):
            list(docs)


class TestMetrics(Scratch):
    def test_each_metric_works(self) -> None:
        db = self.open()
        for name, metric in [
            ("cosine", vdb.Metric.COSINE),
            ("l2", vdb.Metric.L2),
            ("dot", vdb.Metric.DOT),
        ]:
            with self.subTest(metric=name):
                c = db.collection(name, dimension=3, metric=metric)
                c.upsert("a", [1.0, 0.0, 0.0])
                c.upsert("b", [0.0, 1.0, 0.0])
                hits = c.search([1.0, 0.0, 0.0], k=2)
                self.assertEqual(len(hits), 2)
                self.assertEqual(hits[0].id, "a")


class TestContextManagers(Scratch):
    def test_a_database_closes_itself(self) -> None:
        with vdb.open(self.dir) as db:
            db.collection("docs", dimension=3).release()
        self.assertFalse(db.is_open)

    def test_a_collection_releases_itself(self) -> None:
        db = self.open()
        with db.collection("docs", dimension=3) as docs:
            docs.upsert("a", [1.0, 0.0, 0.0])
        with self.assertRaises(vdb.VdbError):
            len(docs)


if __name__ == "__main__":
    unittest.main()
