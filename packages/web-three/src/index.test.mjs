// Verification tests for @anthroforge/web-three, run with:
//   node --test src/index.test.mjs
//
// This reuses the exact fetch-shim pattern from
// packages/web/src/index.test.mjs: @anthroforge/web's init() always
// fetches its bundled wasm and the caller's Part Pack via fetch(), and
// this Node's fetch (undici) does not implement the `file:` scheme, so a
// real local static HTTP server is started over @anthroforge/web's own
// dist/ directory and a thin shim rewrites `file://.../dist/<name>` to
// `http://127.0.0.1:<port>/<name>` before handing off to the real fetch.
// Every byte still travels over a genuine HTTP request/response; the shim
// only bridges the URL-scheme gap.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as THREE from "three";
import { buildHierarchyFixturePack } from "./fixtures/hierarchy-pack.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// @anthroforge/web's own built dist/ — this is what init()'s bundled-wasm
// URL (derived from *its* import.meta.url) resolves against, and it's
// also where we serve the real Part Pack from.
const webDistDir = path.join(__dirname, "..", "..", "web", "dist");

let server;
let baseUrl;

before(async () => {
  server = http.createServer((req, res) => {
    const filePath = path.join(webDistDir, decodeURIComponent(req.url ?? "/"));
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

test("toBufferGeometry() converts a real generate() result into a real THREE.BufferGeometry", async () => {
  const { init, generate } = await import("@anthroforge/web");
  const { toBufferGeometry } = await import("../dist/index.js");

  await init({
    partPackUrl: `${baseUrl}/real_test.afpp`,
    licenseKey: "test-license-key",
  });

  const character = generate({
    seed: 12345n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1001,
    torsoId: 2001,
    clothingIds: [],
  });
  assert.notEqual(character, null, "generate() should return a real character");

  const geometry = toBufferGeometry(character);

  assert.ok(geometry instanceof THREE.BufferGeometry, "should return a real THREE.BufferGeometry");

  const position = geometry.getAttribute("position");
  const normal = geometry.getAttribute("normal");
  const uv = geometry.getAttribute("uv");
  assert.ok(position, '"position" attribute should exist');
  assert.ok(normal, '"normal" attribute should exist');
  assert.ok(uv, '"uv" attribute should exist');
  assert.equal(position.itemSize, 3);
  assert.equal(normal.itemSize, 3);
  assert.equal(uv.itemSize, 2);

  assert.ok(geometry.index, "geometry should have an index");
  assert.equal(geometry.index.count, character.indices.length);

  assert.notEqual(geometry.boundingSphere, null, "computeBoundingSphere() should have run");
});

test("hasAtlas() returns false against a real generate() result (atlas generation is not wired into generate() yet)", async () => {
  const { generate } = await import("@anthroforge/web");
  const { hasAtlas } = await import("../dist/index.js");

  const character = generate({
    seed: 999n,
    heightModifier: 1.0,
    weightModifier: 1.0,
    headId: 1002,
    torsoId: 2002,
    clothingIds: [],
  });
  assert.notEqual(character, null, "generate() should return a real character");

  // This assertion is expected to start failing the day
  // generate_character/generate() actually wires in atlas generation --
  // that's the correct behavior: it means this adapter needs revisiting,
  // not that the real behavior regressed.
  assert.equal(hasAtlas(character), false);
});

// ---------------------------------------------------------------------
// Phase 4: real THREE.Skeleton / THREE.SkinnedMesh support.
//
// The checked-in `real_test.afpp` (served above) has only a single
// identity-transform, parentless "root" joint, which would pass even a
// completely broken hierarchy-builder or a skipped updateMatrixWorld()
// call silently -- identity bones deform identically whether the code is
// right or wrong. These tests instead load a real, non-trivial 3-joint
// (root -> spine -> head) fixture, built fresh in JS by
// `buildHierarchyFixturePack()` (mirroring rust-core's own
// `build_hierarchy_test_glb` unit-test fixture and its assertions
// byte-for-byte), served from its own dedicated one-off HTTP server (the
// pack only exists in memory, not on disk, so it cannot be served from
// the shared `webDistDir` static server above).
// ---------------------------------------------------------------------

/**
 * Serves `bytes` at `/<name>` from a fresh one-off local HTTP server and
 * returns `{ url, close }`. Used for the in-memory-only fixture pack
 * built by `buildHierarchyFixturePack()`, which has no on-disk file to
 * point the shared `webDistDir` server at.
 */
async function serveBytesOnce(name, bytes) {
  const server = http.createServer((req, res) => {
    if (decodeURIComponent(req.url ?? "/") === `/${name}`) {
      res.writeHead(200);
      res.end(Buffer.from(bytes));
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  return {
    url: `http://127.0.0.1:${address.port}/${name}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

test("getSkeleton() against the real non-trivial fixture returns the exact 3 joints it was built with", async () => {
  const { init, getSkeleton } = await import("@anthroforge/web");
  const { packBytes, expectedJoints } = buildHierarchyFixturePack();

  const packServer = await serveBytesOnce("hierarchy_fixture.afpp", packBytes);
  try {
    await init({
      partPackUrl: packServer.url,
      licenseKey: "test-license-key",
    });

    const joints = getSkeleton();
    assert.notEqual(joints, null, "getSkeleton() should return real data");
    assert.equal(joints.length, 3, "expected exactly 3 joints (root, spine, head)");

    const tol = 1e-4;
    const assertVecClose = (actual, expected, label) => {
      assert.equal(actual.length, expected.length, `${label} length mismatch`);
      for (let i = 0; i < expected.length; i++) {
        assert.ok(
          Math.abs(actual[i] - expected[i]) < tol,
          `${label}[${i}]: expected ${expected[i]}, got ${actual[i]}`,
        );
      }
    };

    joints.forEach((joint, i) => {
      const expected = expectedJoints[i];
      assert.equal(joint.parentIndex, expected.parentIndex, `joints[${i}].parentIndex`);
      assertVecClose(joint.translation, expected.translation, `joints[${i}].translation`);
      assertVecClose(joint.rotation, expected.rotation, `joints[${i}].rotation`);
      assertVecClose(joint.scale, expected.scale, `joints[${i}].scale`);
    });
  } finally {
    await packServer.close();
  }
});

test("toSkinnedMesh() builds a real, correctly-bound THREE.SkinnedMesh from the real non-trivial fixture", async () => {
  const { init, generate, getSkeleton } = await import("@anthroforge/web");
  const { toSkinnedMesh } = await import("../dist/index.js");
  const { packBytes, headPartId, torsoPartId } = buildHierarchyFixturePack();

  const packServer = await serveBytesOnce("hierarchy_fixture.afpp", packBytes);
  try {
    await init({
      partPackUrl: packServer.url,
      licenseKey: "test-license-key",
    });

    const skeleton = getSkeleton();
    assert.notEqual(skeleton, null);

    const character = generate({
      seed: 7n,
      heightModifier: 1.0,
      weightModifier: 1.0,
      headId: headPartId,
      torsoId: torsoPartId,
      clothingIds: [],
    });
    assert.notEqual(character, null, "generate() should return a real character");

    const mesh = toSkinnedMesh(character, skeleton);

    assert.ok(mesh instanceof THREE.SkinnedMesh, "should return a real THREE.SkinnedMesh");
    assert.ok(mesh.skeleton instanceof THREE.Skeleton, "mesh.skeleton should be a real THREE.Skeleton");
    assert.equal(mesh.skeleton.bones.length, 3, "expected exactly 3 bones");

    // Parent/child relationships in the actual THREE.Bone objects must
    // match what parentIndex said they should be.
    const bones = mesh.skeleton.bones;
    assert.equal(bones[0].parent, mesh, "root bone (parentIndex -1) should be parented directly to the mesh");
    assert.equal(bones[1].parent, bones[0], "spine's parent should be the root bone");
    assert.equal(bones[2].parent, bones[1], "head's parent should be the spine bone");

    // Directly tests the updateMatrixWorld() requirement, not just the
    // absence of a crash: if THREE.Skeleton's calculateInverses() ran
    // before the bones' matrixWorld was updated, every boneInverse would
    // silently come out as identity (all-zero translation), even though
    // the bind-pose translations built into this fixture are non-zero
    // for bones 1 (spine) and 2 (head).
    const spineInverse = mesh.skeleton.boneInverses[1];
    const spineInverseTranslation = new THREE.Vector3().setFromMatrixPosition(spineInverse);
    assert.notEqual(
      spineInverseTranslation.length(),
      0,
      "spine's boneInverse must not be identity -- if this is zero, updateMatrixWorld() was " +
        "skipped before constructing THREE.Skeleton and the mesh would silently never deform",
    );

    const headInverse = mesh.skeleton.boneInverses[2];
    const headInverseTranslation = new THREE.Vector3().setFromMatrixPosition(headInverse);
    assert.notEqual(headInverseTranslation.length(), 0, "head's boneInverse must not be identity either");
  } finally {
    await packServer.close();
  }
});
