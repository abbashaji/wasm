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
function toSkinnedMesh(character, skeleton, material) {
  const geometry = toBufferGeometry(character);
  const bones = skeleton.map((joint) => {
    const bone = new THREE.Bone();
    bone.position.fromArray(joint.translation);
    bone.quaternion.fromArray(joint.rotation);
    bone.scale.fromArray(joint.scale);
    return bone;
  });
  const roots = [];
  skeleton.forEach((joint, i) => {
    if (joint.parentIndex !== -1) {
      bones[joint.parentIndex].add(bones[i]);
    } else {
      roots.push(bones[i]);
    }
  });
  if (roots.length !== 1) {
    console.warn(
      `toSkinnedMesh: expected exactly 1 root bone (parentIndex === -1), found ${roots.length}`
    );
  }
  for (const root of roots) {
    root.updateMatrixWorld(true);
  }
  const skeleton3 = new THREE.Skeleton(bones);
  const mesh = new THREE.SkinnedMesh(geometry, material);
  for (const root of roots) {
    mesh.add(root);
  }
  mesh.bind(skeleton3);
  return mesh;
}
export {
  hasAtlas,
  toBufferGeometry,
  toSkinnedMesh
};
