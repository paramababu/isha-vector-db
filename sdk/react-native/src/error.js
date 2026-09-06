/**
 * An error carrying the engine's own structured code.
 *
 * Its own module because both the API and the filter compiler throw it, and having the compiler
 * import the API to get at it would make the dependency point the wrong way.
 *
 * The code is the stable one from `docs/api/error-codes.md`, shared by every binding — a caller
 * that branches on "collection not found" branches on the same number here as in Node, Swift or
 * Java. Zero means the failure never reached the engine: a closed handle, a bad argument caught
 * in JavaScript, a native module that is not installed.
 */
export class VdbError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'VdbError';
    /** The stable numeric code. */
    this.code = code;
  }
}
