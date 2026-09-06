// Copy the C ABI header into the package, so the tarball is self-contained.
//
// Both `android/CMakeLists.txt` and the podspec put `include/` on the header search path, and
// `cpp/vdb_bridge.cpp` includes `vdb.h` from it. In 0.1.0 the header was neither shipped nor
// copied, so a build inside an app failed on a missing include — one of the two reasons that
// version could not be installed.
//
// A copy rather than a checked-in duplicate: two copies of a frozen header in one repository is
// one copy that can go stale, and the one that would go stale is the one nobody compiles.
//
// npm runs this on `npm pack` and `npm publish`, so it happens whether the release workflow
// remembers to or not.

import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const packageRoot = join(here, '..');
const repoRoot = join(packageRoot, '..', '..');

const source = join(repoRoot, 'crates', 'isha-vector-db-ffi', 'include', 'vdb.h');
const destination = join(packageRoot, 'include', 'vdb.h');

mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);

console.log(`prepack: copied vdb.h into ${destination}`);
