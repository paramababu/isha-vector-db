/**
 * An embedded, offline-first vector database.
 *
 * These declarations are the canonical shape of the JavaScript API. `@isha-vector-db/web` and
 * `@isha-vector-db/react-native` mirror them exactly: naming adapts to each platform's conventions, but
 * semantics, argument order, defaults and error classification never do. Semantic divergence
 * between SDKs is the fastest way to make a cross-platform library untrustworthy.
 */

/** How aggressively writes are made durable. */
export type Durability =
  /** Sync every write. Safe against power loss; slow on flash. */
  | 'full'
  /**
   * Sync on batch commit, flush and close. The default.
   *
   * In every mode a process crash loses nothing — the bytes are in the page cache. Only power
   * loss can lose an unsynced write, and on a phone process death is routine while power loss
   * is rare.
   */
  | 'batch'
  /** Sync on flush and close only. For bulk import. */
  | 'relaxed';

/** Similarity metric. */
export type Metric =
  /** Cosine similarity. Ignores magnitude, which is usually what embeddings want. */
  | 'cosine'
  /** Euclidean distance. */
  | 'l2'
  /**
   * Inner product. Rewards magnitude as well as direction, so a longer vector can outrank an
   * exact match — that is what the inner product means, not a defect.
   */
  | 'dot';

export interface OpenOptions {
  /** Create the database if the directory holds none. Defaults to true. */
  createIfMissing?: boolean;
  /** Open without the write lock and refuse every mutation. Defaults to false. */
  readOnly?: boolean;
  /** Defaults to `'batch'`. */
  durability?: Durability;
  /** Flush a collection's buffer into a segment past this many bytes. */
  flushThresholdBytes?: number;
}

export interface CollectionOptions {
  /** Vector dimension. Fixed for the collection's lifetime. */
  dimension: number;
  /** Defaults to `'cosine'`. */
  metric?: Metric;
}

/** Metadata values. Flat scalars for now; nested objects and arrays are not yet supported. */
export type MetadataValue = string | number | boolean | null;
export type MetadataInput = Record<string, MetadataValue>;

/**
 * A metadata predicate, written as a query object.
 *
 * ```js
 * { category: 'tools', price: { $lt: 50 } }              // both must hold
 * { $or: [{ category: 'toys' }, { price: { $gt: 50 } }] }
 * { $not: { archived: true } }
 * { tags: { $contains: 'sharp' } }                        // array membership, not substring
 * { price: { $exists: true } }
 * ```
 *
 * A bare value means equality, and several keys in one object mean conjunction — which is what
 * the shape looks like it means. `{}` matches everything.
 *
 * Evaluation is **total**: comparing a string to a number is `false`, never an error, and a
 * field no document has is absent. Three rules surprise people and are worth knowing:
 * an absent field equals `null`; `$ne` is the exact negation of equality so it matches absent
 * fields; and `$gt` and `$lte` are *both* false where no ordering exists, so they are not
 * negations of one another. See `docs/api/filters.md`.
 */
export type Filter = {
  $and?: Filter[];
  $or?: Filter[];
  $not?: Filter;
} & {
  [field: string]: MetadataValue | FieldPredicate | Filter[] | Filter | undefined;
};

export interface FieldPredicate {
  $eq?: MetadataValue;
  /** The exact negation of `$eq`, so it matches documents lacking the field. */
  $ne?: MetadataValue;
  $gt?: MetadataValue;
  $gte?: MetadataValue;
  $lt?: MetadataValue;
  $lte?: MetadataValue;
  $in?: MetadataValue[];
  $nin?: MetadataValue[];
  /** True: the field is present, including an explicit null. False: absent or null. */
  $exists?: boolean;
  /** The field is a string with this prefix. */
  $startsWith?: string;
  /** The field is an **array** containing this value. Not a substring test. */
  $contains?: MetadataValue;
}

export interface Hit {
  id: string;
  /** Always higher-is-better, whatever the metric. */
  score: number;
  /** The metric-native distance. Absent for `'dot'`, which defines none. */
  distance?: number;
}

export interface CollectionStats {
  liveDocuments: number;
  /** Rows on disk, tombstones included. */
  totalRows: number;
  segments: number;
  /** Documents written but not yet folded into a segment. */
  bufferedDocuments: number;
  /** Fraction of rows that are tombstones, 0 to 1. Compaction reclaims them. */
  deadRatio: number;
}

export declare class Collection {
  readonly name: string;
  readonly dimension: number;
  /** Insert or replace. Returns true when the document was new. */
  upsert(id: string, vector: Float32Array, metadata?: MetadataInput): boolean;
  /** Remove a document. Returns whether it existed; removing an absent one is not an error. */
  delete(id: string): boolean;
  contains(id: string): boolean;
  count(): number;
  /**
   * Ordered by score descending, ties broken by ascending id.
   *
   * `topK` counts *matches*, not candidates: a filter excluding most of the collection still
   * returns up to `topK` results.
   */
  search(query: Float32Array, topK: number, filter?: Filter): Hit[];
  flush(): void;
  stats(): CollectionStats;
}

export interface VerifyReport {
  /** Problems meaning data is damaged or unreadable. */
  errors: number;
  /** Things that are odd but not damage — orphan files, an unusually high dead ratio. */
  warnings: number;
  /** The error messages, so you can log or report them. */
  messages: string[];
}

export type VerifyLevel =
  /** Headers and the manifest. Milliseconds, whatever the size. */
  | 'quick'
  /** Every block's checksum. Reads every byte. The default. */
  | 'checksums'
  /** Checksums plus cross-file consistency. */
  | 'full';

export declare class Database {
  readonly isOpen: boolean;
  /** Create a collection, or open it if one exists with a matching shape. */
  collection(name: string, options: CollectionOptions): Collection;
  openCollection(name: string): Collection;
  /** Delete a collection and everything in it. Irreversible. */
  dropCollection(name: string): void;
  listCollections(): string[];
  flush(): void;
  /**
   * Reclaim the space held by tombstoned rows, returning how many were removed.
   *
   * Explicit rather than automatic: rewriting hundreds of megabytes is a decision about when to
   * spend I/O, and your application knows more about that than the engine does. Use a
   * collection's `deadRatio` to decide.
   *
   * @param minDeadRatio how dead a segment must be before it is rewritten; 0 rewrites all.
   */
  compact(minDeadRatio?: number): number;
  /** Check integrity. Reports rather than repairs — a damaged database is a result, not a throw. */
  verify(level?: VerifyLevel): VerifyReport;
  /** Flush and close, releasing the lock. Idempotent. */
  close(): void;
  [Symbol.dispose](): void;
}

/** Open or create a database at a directory path. */
export declare function open(path: string, options?: OpenOptions): Database;
