// The package's entry point: install the bindings, then re-export the API.
//
// # Why this is a separate file from index.js
//
// `index.js` must not import `react-native`. It is run in Node by `npm test`, against a mock
// host object, and that is what makes the filter compiler, the batch packing and the
// handle-state rules testable without a device — the majority of this SDK's own logic. An import
// of `react-native` at the top of it would end that.
//
// So the one line that needs React Native lives here, and `main` points at this file. Nothing
// else changes: everything below is `index.js`.
//
// # Why install runs at import rather than lazily
//
// `install()` is a blocking synchronous native method, so calling it here means
// `globalThis.__vdb` exists before any caller can look for it. Doing it lazily on first `open()`
// would work equally well until someone read `globalThis.__vdb` themselves, which people do when
// they are debugging exactly this.

import { NativeModules } from 'react-native';

// Guarded rather than assumed. A missing module, a failed install, or an app that already
// installed on a previous reload all end up here, and none of them should throw at import time:
// the error that helps is the one `index.js` raises when the API is actually used, which knows
// to mention development builds and Expo Go.
if (globalThis.__vdb === undefined) {
  try {
    NativeModules.Vdb?.install?.();
  } catch {
    // Deliberately swallowed. See above.
  }
}

export * from './index.js';
