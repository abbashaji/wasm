// src/index.ts
import * as THREE from "three";
function toBufferGeometry(character) {
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute("position", new THREE.BufferAttribute(character.positions, 3));
  geometry.setAttribute("normal", new THREE.BufferAttribute(character.normals, 3));
  geometry.setAttribute("uv", new THREE.BufferAttribute(character.uvs, 2));
  geometry.setAttribute("skinIndex", new THREE.BufferAttribute(character.boneIndices, 4));
  geometry.setAttribute("skinWeight", new THREE.BufferAttribute(character.boneWeights, 4));
  geometry.setIndex(new THREE.BufferAttribute(character.indices, 1));
  geometry.computeBoundingSphere();
  return geometry;
}
function hasAtlas(character) {
  return character.atlasWidth > 0 && character.atlasHeight > 0;
}
export {
  hasAtlas,
  toBufferGeometry
};
