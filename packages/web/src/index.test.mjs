// Verification tests for the AnthroForge/Web SDK core, run with:
//   node --test src/index.test.mjs
//
// Environment note on how these tests reach fetch():
// -----------------------------------------------------------------------
// `init()` uses `fetch()` for both the bundled `.wasm` module and the
// caller's Part Pack, because that is the one code path that also works in
// a browser (a plain `fs.readFile` would not exercise it). This Node
// version's `fetch` (undici) does not implement the `file:` scheme at all
// ("not implemented... yet..." is the literal error), and this Node
// version does not support `--experimental-network-imports` either, so the
// built `dist/index.js` module is loaded the normal (file-based) ESM way,
// which means its `import.meta.url` — and therefore the wasm URL it
// derives from it — is unavoidably a `file://` URL.
//
// To let `fetch()` actually succeed against that URL without silently
// swapping in `fs.readFile`, this test starts a real local static HTTP
// server over `dist/` and wraps `globalThis.fetch` with a thin shim that
// rewrites `file://.../dist/<name>` to `http://127.0.0.1:<port>/<name>`
// before handing off to the *real* fetch. The Part Pack URL passed to
// `init()` is already an `http://` URL pointing at the same server, so it
// passes through the shim untouched. Every byte in this test still travels
// over a genuine HTTP request/response to a genuine server — the shim only
// bridges the URL-scheme gap created by Node's lack of network-import
// support in this version, not the fetch mechanism itself.
// -----------------------------------------------------------------------

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const distDir = path.join(__dirname, "..", "dist");

let server;
let baseUrl;

before(async () => {
  server = http.createServer((req, res) => {
    const filePath = path.join(distDir, decodeURIComponent(req.url ?? "/"));
    fs.readFile(filePath, (err, data) => {
      if (err) {
        res.writeHead(404);
        res.end();
        return;
      }
      res.writeHead(200);
      res.end(data);
    });
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  baseUrl = `http://127.0.0.1:${address.port}`;

  const realFetch = globalThis.fetch;
  globalThis.fetch = (input, init) => {
    const url = typeof input === "string" ? new URL(input) : new URL(input.url ?? input);
    if (url.protocol === "file:") {
      const fileName = path.basename(url.pathname);
      return realFetch(`${baseUrl}/${fileName}`, init);
    }
    return realFetch(input, init);
  };
});

after(async () => {
  await new Promise((resolve) => server.close(resolve));
});

test("init() loads the real wasm module and the real .afpp pack", async () => {
  const { init } = await import("../dist/index.js");
  await assert.doesNotReject(
    init({
      partPackUrl: `${baseUrl}/real_test.afpp`,
      licenseKey: "test-license-key",
    }),
  );
});

test("generate() with valid DNA and real part ids returns a non-null mesh", async () => {
  const { generate } = await import("../dist/index.js");

  // head_id=1001 and torso_id=2001 are real part ids present in
  // real_test.afpp, confirmed by parsing the pack's part table directly
  // (id/tag/offset/length entries at file offset 0x4d) and cross-checked
  // by calling generate_character against the live wasm module: setting
  // out-of-range ids (0-3) produced "no part loaded for head_id <N>" with
  // N echoing exactly what was set, and these two real ids succeed.
  const result = generate({
    seed: 12345n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1001,
    torsoId: 2001,
    clothingIds: [],
  });

  assert.notEqual(result, null);
  assert.ok(result.positions.length > 0, "positions should be non-empty");
  assert.ok(result.indices.length > 0, "indices should be non-empty");
  assert.equal(result.positions.length % 3, 0);
  assert.equal(result.normals.length, result.positions.length);
  assert.equal(result.uvs.length, (result.positions.length / 3) * 2);
  assert.equal(result.boneIndices.length, (result.positions.length / 3) * 4);
  assert.equal(result.boneWeights.length, (result.positions.length / 3) * 4);
  assert.equal(result.atlasBytes.length, 0);
  assert.equal(result.atlasWidth, 0);
  assert.equal(result.atlasHeight, 0);
});

test("generate() with invalid DNA (heightModifier: 0) returns null and sets a real error", async () => {
  const { generate, getLastError } = await import("../dist/index.js");

  const result = generate({
    seed: 12345n,
    heightModifier: 0,
    weightModifier: 1.0,
    headId: 1001,
    torsoId: 2001,
    clothingIds: [],
  });

  assert.equal(result, null);
  const err = getLastError();
  assert.notEqual(err, null);
  assert.ok(err.length > 0, "error string should be non-empty");
});

test("freeCharacter() is a callable no-op", async () => {
  const { generate, freeCharacter } = await import("../dist/index.js");
  const result = generate({
    seed: 999n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1002,
    torsoId: 2002,
    clothingIds: [],
  });
  assert.notEqual(result, null);
  assert.doesNotThrow(() => freeCharacter(result));
});

// This test exists specifically because generate_character's clothing
// integration (equipped_clothing_ids_ptr -> fit -> merge into one output
// mesh) was, until this test, wired on both the Rust side and the JS
// marshalling side but never actually exercised end-to-end by anything in
// this project -- a "should work" gap the project's own methodology exists
// to catch before it ships. Part id 1002 ("legs.obj") is reused here
// purely as a stand-in equippable item, not because it's semantically
// clothing -- the point is to prove the pipeline actually runs and actually
// changes the output, not to validate real garment fitting quality.
test("generate() with a real equipped clothingId actually merges it into the output", async () => {
  const { generate, getLastError } = await import("../dist/index.js");

  const bodyOnly = generate({
    seed: 42n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1001,
    torsoId: 2001,
    clothingIds: [],
  });
  assert.notEqual(bodyOnly, null, `body-only generate() failed: ${getLastError()}`);

  const withClothing = generate({
    seed: 42n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1001,
    torsoId: 2001,
    clothingIds: [1002],
  });
  assert.notEqual(
    withClothing,
    null,
    `generate() with clothingIds:[1002] returned null (real error: ${getLastError()}); ` +
      "this means the clothing path failed rather than being skipped (a skip only " +
      "drops that one item and still returns the body, per generate_character's own " +
      "doc comment) -- if this fails, read the real anthroforge_last_error() text " +
      "above before assuming a code bug, since a missing/unfittable part id is also " +
      "a possible real cause.",
  );

  // The whole point of this test: clothing must actually change the
  // output, not just fail to error. A vertex/index count increase is the
  // cheapest real signal that clothing_id 1002's geometry actually got
  // merged in, without needing to inspect individual vertex positions.
  assert.ok(
    withClothing.positions.length > bodyOnly.positions.length,
    `expected withClothing to have MORE vertex data than bodyOnly ` +
      `(bodyOnly: ${bodyOnly.positions.length / 3} verts, ` +
      `withClothing: ${withClothing.positions.length / 3} verts) -- if these are ` +
      'equal, clothing_id 1002 was silently skipped (check stderr for a ' +
      '"skipping this item" message from generate_character) rather than merged.',
  );
  assert.ok(withClothing.indices.length > bodyOnly.indices.length);
});
