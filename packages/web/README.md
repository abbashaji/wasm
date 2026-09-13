# @anthroforge/web

WebAssembly-powered character generation for the browser.

## Install

```sh
npm install @anthroforge/web
```

## Usage

```ts
import { init, generate, getLastError } from "@anthroforge/web";
import type { CharacterDNA, GeneratedCharacter, InitOptions } from "@anthroforge/web";

const options: InitOptions = {
  // see type definitions for available options
};

await init(options);

const dna: CharacterDNA = {
  // ...
};

const character: GeneratedCharacter | null = generate(dna);

if (!character) {
  console.error(getLastError());
}
```

When you're done with a generated character, release its underlying
wasm-side memory:

```ts
import { freeCharacter } from "@anthroforge/web";

freeCharacter(character);
```

## Size

Real, measured numbers from `npm run build && npm run measure-size`
(2026-09-13):

| Artifact | Size |
|---|---|
| Raw `.wasm` (Rust build output) | 781,375 bytes (763.06 KB) |
| Optimized `.wasm` (after `optimize-wasm.mjs`) | 483,306 bytes (471.98 KB) — 38.1% reduction |
| Bundled JS (`dist/index.js`) | 8,172 bytes (7.98 KB) |
| **Combined package size** (wasm + js, what a consumer's bundler actually fetches) | **491,478 bytes (479.96 KB)** |

## License

TBD
