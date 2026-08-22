// A storage adapter over node:fs, for testing the web stack without a browser.
//
// This is not shipped to a browser and is not a supported way to run vdb on a server — use the
// native Node addon in `sdk/node` for that, which is faster and has no WebAssembly boundary.
// What this exists for is to let the host marshalling layer, the wasm module, and the whole
// engine be tested in ordinary CI, so the browser only has to prove the one thing it uniquely
// can: that OPFS behaves the way `host.js` expects.
//
// Every method is synchronous, which is the same shape OPFS sync access handles have.

import * as fs from 'node:fs';
import * as path from 'node:path';

export function nodeAdapter(root) {
  const resolve = (p) => path.join(root, p.replace(/^\/+/, ''));

  return {
    stat(p) {
      try {
        const st = fs.statSync(resolve(p));
        return { size: st.size, directory: st.isDirectory() };
      } catch {
        return null;
      }
    },

    open(p, writable) {
      const full = resolve(p);
      // 'r' cannot create; 'r+' needs the file to exist. The host has already decided whether
      // creation is allowed, so by here the only question is read-only or read-write.
      if (!writable) {
        const fd = fs.openSync(full, 'r');
        return handle(fd);
      }
      if (!fs.existsSync(full)) fs.writeFileSync(full, Buffer.alloc(0));
      return handle(fs.openSync(full, 'r+'));
    },

    removeFile(p) {
      fs.unlinkSync(resolve(p));
    },

    createDirAll(p) {
      fs.mkdirSync(resolve(p), { recursive: true });
    },

    removeDirAll(p) {
      fs.rmSync(resolve(p), { recursive: true, force: true });
    },

    syncDir(_p) {
      // node:fs cannot fsync a directory portably, and this adapter exists for tests rather
      // than durability. The backend already declares `durable_sync: false`, so nothing in the
      // engine is relying on this being a barrier.
    },

    listDir(p) {
      return fs.readdirSync(resolve(p), { withFileTypes: true }).map((e) => ({
        name: e.name,
        directory: e.isDirectory(),
      }));
    },
  };
}

function handle(fd) {
  return {
    read(dest, offset) {
      // `dest` is a view into WebAssembly linear memory. Reading straight into it avoids a copy,
      // but the view must not be retained: it is invalidated the moment memory grows.
      return fs.readSync(fd, dest, 0, dest.length, offset);
    },
    write(src, offset) {
      let written = 0;
      while (written < src.length) {
        written += fs.writeSync(fd, src, written, src.length - written, offset + written);
      }
    },
    truncate(len) {
      fs.ftruncateSync(fd, len);
    },
    size() {
      return fs.fstatSync(fd).size;
    },
    flush() {
      fs.fsyncSync(fd);
    },
    close() {
      fs.closeSync(fd);
    },
  };
}
