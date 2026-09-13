# AnthroForge/Web — Product Specification

*A browser-embeddable, engine-agnostic character-generation SDK.*

> **How to read this document.** This is a standalone product spec, written to be
> read once by an engineer, a marketplace reviewer, or a buyer evaluating the SDK,
> with no prior context assumed. It covers what the product is, why the market gap
> is real, what changes are required in the existing Rust core before this can ship,
> and how it's packaged and sold. §9 states the known WASM-specific gaps that
> must close before v1 (revised in v2 of this spec — see the revision note below).

> **Relationship to prior work.** This is not a new engine. It is the existing
> `rust-core` crate (originally built as the computational core of an Unreal Engine
> plugin) recompiled to a second target — WebAssembly — and re-packaged as a
> standalone JavaScript/TypeScript SDK with no Unreal, no Rust toolchain, and no
> native binary required on the consumer's side. The generation logic does not
> change; the target, packaging, and buyer do.

> **Revision note (v2).** The first draft of this spec's §9 gap list was written
> against the project's Phase 5 `README.md`, which was already stale by the time
> this spec was drafted. Cross-checked directly against the actual
> `rust-core/src/lib.rs`/`error.rs`/`Cargo.toml` and the Phase 6–9 merge notes in
> the engine handoff package, two of the four "must-resolve" blockers were already
> closed before this spec was written: clothing has been wired into
> `generate_character` since Phase 6 (confirmed by a real UE5.8 automation pass —
> `EquippedClothingIsComposedIn: Success`), and FFI error codes have existed since
> Phase 5 via `anthroforge_last_error()`. §4.2, §5, §9, and §10 below are corrected
> accordingly. One new, genuinely WASM-specific risk is added in its place (see
> §9.4) that the v1 draft didn't surface at all: `panic = "unwind"`, the setting
> the native crate already uses, is not guaranteed to behave the same way once
> compiled to `wasm32-unknown-unknown`, and this needs to be tested empirically,
> not assumed either way.

---

## 0. One-paragraph summary

AnthroForge/Web is a client-side JavaScript SDK — a `.wasm` binary plus a thin
TypeScript wrapper — that generates parametric human character meshes, textures,
material variants, and fitted clothing entirely inside a web browser, with no
server round-trip, no API key, no per-generation billing, and no dependency on
any game engine. It targets
a buyer Unreal's Mutable structurally cannot reach: web-based avatar creators,
browser RPGs, VTuber/streaming-avatar tools, and "digital identity" products that
need character generation to run in a browser tab, not inside Unreal. The product
is sold once, as a license, through npm and a direct storefront — no hosting, no
recurring infrastructure cost, and no dependency on any marketplace's review queue
or reputation system for discovery.

---

## 1. Why this system exists (market gap, briefly)

Every competitor in procedural-human-generation is coupled to something the buyer
must already own or run:

| Competitor | Coupling |
|---|---|
| Unreal's Mutable | Requires the Unreal Editor and a UE runtime; cannot execute in a browser or Node process. |
| AnthroForge (UE plugin form) | Requires Unreal Engine 5.8+; same limitation as Mutable for this buyer. |
| Ready Player Me | Cloud API — every avatar generation is a network call to a third-party server the buyer doesn't control, with per-call cost and an outage dependency. |
| DiceBear / similar 2D avatar libraries | Browser-native, but flat 2D/SVG sprite avatars, not 3D meshes — a different product category. |

**The gap:** there is no browser-native, offline (no network call per generation),
3D character-mesh SDK on the market today. A web developer building an avatar
creator, a browser-based RPG, or a streaming-avatar tool currently has exactly two
bad options: embed a full game engine (Unity WebGL export, at enormous size and load
cost) or call a metered cloud API (Ready Player Me and similar) and accept per-user,
per-generation billing and an external dependency for something that could run
entirely client-side.

AnthroForge/Web fills that gap because the underlying generation logic already has
no GPU dependency, no engine dependency, and a C-ABI surface — it is, by construction,
already most of the way to being embeddable anywhere `wasm32` can run.

---

## 2. Core concepts

| Term | Definition |
|---|---|
| **Core** | The existing `rust-core` crate, unmodified in generation logic, compiled to a second target (`wasm32-unknown-unknown`) in addition to its existing native (`cdylib`) target. |
| **Bindings** | A `wasm-bindgen`-generated JS/TS glue layer exposing the Core's `extern "C"` surface as ergonomic JS functions and TypeScript types. |
| **DNA** | The existing `CharacterDNA` struct (seed, height/weight modifiers, head/torso/clothing IDs) — unchanged from the native product, serialized across the JS/WASM boundary as a plain JS object. |
| **Part Pack** | A versioned bundle of base meshes/textures (the existing OBJ + RGBA8 demo-asset format, or a customer-supplied set) shipped alongside the `.wasm` binary, loaded at SDK-init time. |
| **Render Adapter** | A thin, optional integration layer translating the Core's raw output (vertex/index buffers, atlas bytes) into a `three.js`, Babylon.js, or raw WebGL buffer — the only genuinely new code this product needs beyond the WASM build itself. |
| **License Key** | A build-time or init-time string validated locally against a signed manifest — no network call, consistent with the existing "100% offline" design principle inherited from the native product. |

---

## 3. System boundary

### 3.1 Build-time pipeline (developer-side, one-time per release)

```
rust-core (unmodified generation logic)
        │
        ├── existing target: wasm32... no. See below: two build targets
        │     ├── cdylib (native)   → existing Unreal plugin, unchanged
        │     └── wasm32-unknown-unknown (NEW)
        │
        ▼
wasm-bindgen (CLI + macro layer)
        │  generates: anthroforge_core.wasm + anthroforge_core.js + .d.ts
        ▼
   wasm-opt (Binaryen)              ── size/perf pass, §6.2
        │
        ▼
   @anthroforge/web npm package
        │  wasm binary + JS glue + TS types + Render Adapters + Part Pack loader
        ▼
        (published to npm + CDN + direct-download zip for non-npm buyers)
```

### 3.2 Runtime lifecycle (consumer-side, in-browser)

```
   Web page loads @anthroforge/web
        │
        ▼
   AnthroForge.init({ partPackUrl, licenseKey })   — one-time, async (WASM instantiation)
        │
        ▼
   AnthroForge.generate(dna)   — synchronous after init, same sub-100µs-class cost
        │                          as the native core (see §7 for the one caveat)
        ▼
   { vertices, indices, atlasBytes }  — raw typed arrays, engine-agnostic
        │
        ▼
   (optional) Render Adapter → three.js Mesh / Babylon Mesh / raw WebGL buffers
```

No server exists in this lifecycle. No generation call leaves the browser tab.
License validation (§8.3) is local, not a live gate — matching the original native
product's Principle 1 ("no live infrastructure") almost by inheritance, not by new
design work.

---

## 4. Technical architecture, in depth

### 4.1 Compilation target

`wasm32-unknown-unknown`, built via `wasm-bindgen` rather than `wasm-pack`'s
higher-level wrapper, for direct control over the generated glue and to avoid
pulling in `wasm-pack`'s Node-oriented defaults, which assume a build toolchain the
end consumer (a web developer, not a Rust developer) should never need to install.
The published npm package ships **pre-built** `.wasm` and `.js` output — consumers
never invoke `cargo` or `wasm-bindgen` themselves.

### 4.2 What changes in the existing Rust core, and what doesn't

| Module | WASM-compatible as-is? | Required change |
|---|---|---|
| `body_mutation.rs` | Yes | None — pure math over slices, no OS/thread dependency. |
| `mesh_merge.rs` | Yes | None. |
| `obj_loader.rs` / `gltf_loader.rs` | Mostly | File I/O (`std::fs`) must be replaced with a byte-buffer API — WASM has no filesystem by default. Part Packs are fetched by the JS host and passed in as `Uint8Array`, not read from disk. |
| `texture_atlas.rs` | Yes | None — pure `std`, no OS dependency per the existing `README.md`'s own module notes. |
| `clothing_deformer.rs` | Yes, with a real caveat | **Corrected from v1 of this spec.** This module is not blocked — it has been wired into `generate_character` since Phase 6 (per-body anchor caching, per-item fit-or-skip, verified in a real UE5.8 automation run: `EquippedClothingIsComposedIn: Success`). The only actual WASM-specific work left is the `rayon` feature-gate below, since `fit_clothing_to_skin`/`build_cloth_anchors` are where the crate's `par_iter`/`par_iter_mut` calls live. |
| Threading (`rayon`, used by `clothing_deformer.rs`'s anchor-building and clothing-fit passes) | **No**, not without extra work | `wasm32-unknown-unknown` has no native thread support without the `wasm-bindgen-rayon` + `SharedArrayBuffer` + cross-origin-isolation-headers combination, which imposes hosting requirements (`COOP`/`COEP` headers) on every consuming site. **v1 decision: ship single-threaded**, `rayon`'s parallel iterators degrade to sequential automatically when the `rayon` feature is compiled out for the wasm target — no code fork required, only a `Cargo.toml` feature gate. Note this directly affects the clothing-fit path (previous row), not just a hypothetical future feature. |
| `panic = "unwind"` (actual current release profile — **not** `"abort"`, corrected from v1 of this spec) | **Untested under `wasm32-unknown-unknown` specifically — do not assume it transfers** | The native crate was switched from `panic = "abort"` to `panic = "unwind"` back in Phase 5 specifically so `texture_atlas::generate_runtime_atlas`'s `catch_unwind` wrapper could work (verified empirically against native builds in `RESULTS-03.md`). That verification was native-only. Rust's `wasm32-unknown-unknown` target has historically had incomplete/non-default unwinding support — a panic may still trap (`RuntimeError: unreachable`) rather than unwind, regardless of the crate's `panic` profile setting, unless the toolchain and target both support it. This must be compiled and empirically tested against the actual wasm build in real browsers (see §9.4) before any doc claims a specific panic-recovery behavior — the answer could reasonably come back either way. |

### 4.3 Memory and size budget

- WASM linear memory starts small and grows via `memory.grow`; the Part Pack
  (currently ~22MB for the existing demo asset set, per the native product's
  Phase 9b measurement) is **not** embedded in the `.wasm` binary — it is fetched
  separately by the host page and passed to `init()` as raw bytes, exactly mirroring
  how the native plugin already treats `sample-assets/` as external data, not
  compiled-in.
- Target `.wasm` binary size (code only, no Part Pack): **under 500KB post-`wasm-opt`**,
  based on the actual compiled surface (five modules, no GUI/rendering code, no
  `gltf`/`tobj` parsing needed at runtime once Part Packs ship pre-parsed — see §4.4).
- This is a real, checkable engineering target, not an aspirational marketing number,
  and should be measured and stated exactly (not rounded up) once the first build
  exists.

### 4.4 Part Pack format decision

Ship Part Packs **pre-parsed** into the engine's own internal buffer format (the
existing `SkinnedVertex` layout) rather than as raw `.obj`/`.gltf` for the browser
to parse at load time. This removes `gltf_loader.rs`/`obj_loader.rs`'s parsing cost
from the browser's critical path entirely — parsing happens once, at Part-Pack-build
time, on the developer's machine, using the *existing* native build. This is a
genuine, non-obvious reuse of the native toolchain: **the native `cdylib` becomes
the Part Pack authoring tool for the Web SDK**, not a separate thing to build.

### 4.5 Render Adapters

The Core deliberately returns raw typed arrays (`Float32Array` for vertices,
`Uint32Array` for indices, `Uint8Array` for atlas bytes) with **zero rendering
dependency** — this keeps the Core small and engine-agnostic. Three thin,
independently-versioned adapter packages sit on top:

- `@anthroforge/web-three` — wraps output in a `THREE.BufferGeometry` + `THREE.Texture`.
- `@anthroforge/web-babylon` — same, for Babylon.js's `Mesh`/`RawTexture`.
- `@anthroforge/web-raw` — a documented reference implementation for raw WebGL/WebGPU
  buffer upload, for teams on a custom renderer.

Shipping these as separate, optional packages (not bundled into the Core) keeps the
Core's install size minimal for buyers who already have their own render pipeline
and only want the generation math.

---

## 5. API surface (developer-facing)

```typescript
// @anthroforge/web

interface CharacterDNA {
  seed: bigint;
  heightModifier: number;
  weightModifier: number;
  headId: number;
  torsoId: number;
  clothingIds: number[];   // IN v1 — corrected from v1 of this spec. Native
    // `generate_character` has consumed `equipped_clothing_ids_ptr` /
    // `equipped_clothing_count` since Phase 6; the wasm bindings marshal this
    // array to the same pointer+length pair the native FFI already expects.
    // Per-item resolution follows the native skip-not-fail policy: an
    // unresolvable clothing id or a failed anchor build silently omits that
    // one item from the output rather than failing the whole call — callers
    // that need to know why an item didn't render should check
    // `getLastError()` after `generate()`, not assume every id in the array
    // rendered.
}

interface GeneratedCharacter {
  vertices: Float32Array;   // interleaved SkinnedVertex layout
  indices: Uint32Array;
  atlasBytes: Uint8Array;
  atlasWidth: number;
  atlasHeight: number;
}

interface InitOptions {
  partPackUrl: string;       // fetched by the SDK, not bundled
  licenseKey: string;        // validated locally, no network call
}

declare function init(options: InitOptions): Promise<void>;
declare function generate(dna: CharacterDNA): GeneratedCharacter | null; // null
  // on failure — see getLastError() below for why. (v1 of this spec omitted
  // the null case since it assumed no error surface existed yet.)
declare function getLastError(): string | null; // NEW, corrected from v1 of
  // this spec. Wraps the native `anthroforge_last_error()` export, which has
  // existed since Phase 5 — this was never a gap to close, only a binding to
  // write. Same thread-local, "valid until the next call" contract as the
  // native function; the wasm binding should copy it to a JS string
  // immediately rather than expose the raw pointer, since JS has no
  // equivalent notion of "read this before your next call on this thread."
declare function freeCharacter(character: GeneratedCharacter): void; // explicit,
  // mirrors the native free_mesh_buffer pattern; WASM linear memory leaks
  // silently otherwise, same failure mode as the native leak documented
  // in the existing README, just relocated to the browser tab instead of
  // a game process.
```

This surface is intentionally a near-1:1 mirror of the existing native `extern "C"`
API (`init_part_registry`, `generate_character`, `free_mesh_buffer`) — minimizing
new API-design risk by reusing a contract that's already been exercised by 63
passing native unit tests and a real UE automation run.

---

## 6. Packaging, licensing, and pricing

### 6.1 Distribution channels

| Channel | Purpose |
|---|---|
| npm (`@anthroforge/web`) | Primary distribution for the target buyer (web developers already live in the npm ecosystem). |
| jsDelivr / unpkg CDN | Zero-build-step `<script>` tag usage for no-bundler prototyping — lowers the trial barrier to literally copy-pasting a script tag. |
| Direct download zip (Gumroad or a self-hosted storefront) | For buyers who want an offline, audit-before-use artifact rather than a live npm dependency — a real, stated buyer preference in security-conscious teams. |

### 6.2 Licensing model

- **Per-seat / per-developer-team license**, validated by a signed manifest checked
  locally at `init()` time — no telemetry, no phone-home, consistent with the
  "no live infrastructure" principle inherited from the native architecture.
- **No per-generation or per-end-user billing** — this is the core differentiator
  against Ready Player Me's metered-API model, and it's a real structural
  advantage: once licensed, a buyer can generate unlimited characters for unlimited
  end users at zero marginal cost, something no cloud-API competitor can offer.
- Suggested tiers (indicative, not load-bearing on the technical spec):
  - **Indie** — single project, one npm scope token.
  - **Studio** — unlimited projects under one company entity.
  - **Source** — includes the Rust source and the right to modify/recompile,
    for teams wanting to extend the Domain Adapter set themselves.

### 6.3 Why this pricing model is structurally strong

A one-time or per-seat license with zero marginal generation cost is the single
biggest differentiator this product has against every cloud-API competitor in this
space, and it costs nothing extra to deliver — it falls out of the "no server"
architecture for free. This is worth stating plainly in marketing copy: *"Generate
unlimited characters for unlimited users. No API key limits, no per-avatar billing,
no server to keep online."*

---

## 7. Performance: what changes moving native → WASM

The native product's real, measured numbers (Phase 9a–9c) do not transfer 1:1:

- WASM execution is typically **1.2–3x slower** than native machine code for
  compute-bound Rust, depending on the browser's JIT/AOT tier — this is a widely
  observed, general property of WASM execution, not something specific to this
  codebase, and should be **measured**, not assumed, once a real WASM build exists.
  Do not publish a browser-side µs/character number until it's actually benchmarked
  in-browser; carrying over the native Phase 9 numbers unchanged would repeat the
  exact "unverified claim" mistake already flagged in this codebase's own
  `PHASE_9_FINAL_REPORT.md` about not presenting projections as measurements.
- No `rayon` parallelism in v1 (§4.2) means multi-clothing-item generation, which
  benefited from shared per-body KD-tree amortization in the native benchmarks,
  will scale differently — again, to be measured, not assumed.
- **Honest marketing claim for v1, pending real numbers:** "fast enough for
  real-time avatar customization in a browser — hundreds of characters per second
  on commodity hardware," stated only after an actual browser benchmark replaces
  this placeholder.

---

## 8. Marketing and positioning

### 8.1 Who this is for

- Web-based avatar creator products (character customization screens for browser
  games, "make your avatar" onboarding flows).
- VTuber / streaming-avatar tooling vendors.
- Browser-based RPGs and social/metaverse-adjacent web apps needing 3D character
  variety without a game-engine dependency.
- Teams currently paying per-generation to a cloud avatar API who want to move the
  cost structure to a flat license.

### 8.2 Positioning statement

*"AnthroForge/Web is the only browser-native, offline 3D character-generation SDK.
Everything else in this category either requires a game engine you don't have, or
a cloud API you don't control and pay for per user."*

This claim should be verified against the current competitive landscape (Ready
Player Me's current pricing/architecture, and any newer browser-native entrants)
immediately before launch copy is finalized, since this is a fast-moving space and
the "only" claim is falsifiable — a quick search pass before publishing is cheap
insurance against an easily-disproven marketing claim.

### 8.3 Explicitly do NOT position against Mutable

Per the prior analysis in this conversation: Mutable is not this product's
competitor, and mentioning it in Web SDK marketing copy would be pointless — the
Web SDK's buyer was never going to consider a UE-only plugin. Keep Mutable
comparisons entirely out of this product's marketing; they belong to the (separate,
weaker) native-plugin positioning problem, not this one.

### 8.4 Distribution / discovery channels

- npm search + "3d avatar sdk," "browser character generator" keyword targeting.
- A live, interactive demo page (the single highest-leverage marketing asset for a
  dev-tool SDK — "try it in your browser right now" converts far better than any
  written description) — cheap to build since the Core's whole point is running
  client-side already.
- Dev-tool discovery surfaces: Hacker News "Show HN," r/webdev, r/gamedev, Product
  Hunt — all discovery-driven, reputation-agnostic channels consistent with the
  "no connections needed" constraint from earlier in this analysis.

---

## 9. Known gaps — what actually blocks v1 (corrected from v1 of this spec)

The original draft of this section listed two items ("clothing not shippable,"
"no error codes") that were true of the project's Phase 5 state but had already
been resolved by Phase 6/Phase 5-round-5 respectively by the time this spec was
written — see the Revision note at the top of this document for how that was
caught. Both are removed below and replaced with what's actually still open,
which is smaller in scope but not zero:

1. **`std::fs` calls in `obj_loader.rs`/`gltf_loader.rs` must be replaced with
   byte-buffer inputs before this compiles to `wasm32-unknown-unknown` at all.**
   This is a hard compile-time blocker, not a quality issue — WASM has no
   filesystem by default. Confirmed still present in the current source
   (`gltf_loader.rs`'s buffer-source resolution calls `std::fs::read` directly).
   Straightforward, mechanical work (§4.2), and it's already correctly scoped
   into Phase 1 below.
2. **`rayon`'s `par_iter`/`par_iter_mut` calls in `clothing_deformer.rs` need the
   sequential-fallback feature gate for the wasm target**, or the crate won't
   compile for `wasm32-unknown-unknown` at all without the
   `wasm-bindgen-rayon`/`SharedArrayBuffer`/COOP-COEP stack this spec's v1
   deliberately avoids (§4.2, §11). Since clothing fitting is exactly the code
   path that uses `rayon`, this now sits on the critical path to shipping
   clothing in the Web SDK, not a side concern.
3. **`free_mesh_buffer` has real native evidence behind it now, but nothing
   WASM-specific.** Phase 9c's scale-stress runs (5,000–10,000 characters,
   buffers freed immediately after each) show flat peak RSS with no growth
   attributable to per-character leaks, and the same buffer-free pattern is
   exercised in a live UE5.8 automation test. That's meaningfully stronger
   evidence than "reviewed but never compiled," which is what the original
   README's caveat (and this spec's v1 draft) suggested. What's still
   unverified is WASM linear memory specifically — a leak there degrades a
   long-lived browser tab differently than a game process, and the native
   evidence, while reassuring, doesn't substitute for a real wasm-target test.
4. **NEW — `panic = "unwind"` under `wasm32-unknown-unknown` is untested and
   should not be assumed to work the way it does natively.** The native crate
   already uses `panic = "unwind"` (changed from `"abort"` back in Phase 5,
   specifically so `generate_runtime_atlas`'s `catch_unwind` wrapper works — see
   `RESULTS-03.md`), and that native behavior is empirically verified. But
   `wasm32-unknown-unknown` has historically had incomplete unwinding support
   independent of the crate's own `panic` setting — an internal panic may still
   trap as an unrecoverable `RuntimeError: unreachable` in the browser regardless
   of what `Cargo.toml` says. This replaces the original draft's "`panic =
   "abort"` should be tested" item, which was based on a stale assumption about
   which panic strategy is even in use; the actual open question is different
   and, if anything, more consequential, since it affects whether
   `getLastError()`-style graceful failure (§5) is achievable at all for
   panic-triggered errors versus only for the explicit `Result`-based ones.

None of these are reasons not to build this — they're the actual punch list, and
it's shorter and more tractable than the original draft implied. Two of the four
original items turned out to already be done; this list trades those out for the
work that's genuinely still open.

---

## 10. Build phases

**Phase 1 — Prove the WASM target compiles and runs at all**
- Add `wasm32-unknown-unknown` as a second build target alongside the existing
  `cdylib`, gated so it doesn't disturb the native build.
- Replace `std::fs` calls in `obj_loader.rs`/`gltf_loader.rs` with byte-buffer
  inputs (§4.2).
- Get one `generate_character` call working end-to-end in a browser console
  against one hard-coded Part Pack. No packaging, no npm, no adapters yet — pure
  feasibility proof.

**Phase 2 — Close the remaining WASM-specific gaps (re-scoped — corrected from v1
of this spec, which had this phase opening with FFI error codes that already
exist)**
- Feature-gate `rayon` to sequential for the wasm target (§9.2) — this is now the
  gating item for shipping clothing in the Web SDK, since it's on
  `clothing_deformer.rs`'s critical path.
- Wire `clothingIds`/`equipped_clothing_ids_ptr` through the `wasm-bindgen`
  glue and expose `getLastError()` in the JS bindings (§5) — both wrap
  functionality the native crate already has; this is binding work, not new
  Rust logic.
- Verify `free_mesh_buffer` under a real WASM compile specifically (§9.3).
- Empirically test `panic = "unwind"` behavior under `wasm32-unknown-unknown`
  in real browsers (§9.4) before writing any panic-recovery documentation.
- Build the Part-Pack-authoring pipeline (§4.4) using the existing native binary.

**Phase 3 — Package and benchmark**
- `wasm-bindgen` + `wasm-opt` pipeline, real `.wasm` size measurement (§4.3).
- Real in-browser benchmark numbers (§7) — replace every placeholder performance
  claim with a measured one before writing marketing copy.
- Publish `@anthroforge/web` to npm; build the live interactive demo page.

**Phase 4 — Render Adapters and launch**
- `@anthroforge/web-three` first (largest ecosystem), then Babylon, then the raw
  reference adapter.
- Launch across the discovery channels in §8.4.
- Clothing ships as part of v1 in this plan (corrected from v1 of this spec,
  which deferred it to a Phase 5 pending a native-side fix that turned out to
  already exist) — no separate clothing phase is needed.

---

## 11. Open design decisions (deliberately deferred)

- **Threading strategy for v2.** Whether `SharedArrayBuffer`-based multithreading
  (via `wasm-bindgen-rayon`) is worth the `COOP`/`COEP` hosting-header burden it
  imposes on every consuming site is a real tradeoff, not resolved here — v1 ships
  single-threaded deliberately to avoid forcing a hosting requirement on early
  adopters.
- **License-key validation mechanism, exact form.** A signed manifest checked
  locally is the direction, not the fully specified mechanism — same category of
  open question the native product already deferred for its own local license
  validation, and reasonably deferred here too as a business/legal decision.
- **Whether to bundle a default Part Pack at all**, versus shipping the SDK
  entirely asset-free and requiring every buyer to author or purchase their own —
  a genuine product-scope decision affecting time-to-first-demo for a new buyer,
  intentionally left open pending early user feedback.
- **WebGPU adapter timing.** WebGL adapters (§4.5) are the safe v1 choice for
  compatibility; a WebGPU-native adapter is a plausible future addition once
  WebGPU's browser support matures further, not a v1 requirement.

---

## 12. Glossary

| Term | One-line meaning |
|---|---|
| Core | The Rust generation logic, unchanged, compiled to a second (WASM) target |
| Bindings | Generated JS/TS glue exposing the Core's functions to a web page |
| DNA | The seed + modifier struct describing one character, unchanged from native |
| Part Pack | Pre-parsed mesh/texture data, fetched by the host page, not bundled in the `.wasm` |
| Render Adapter | Optional thin layer translating raw buffers into a specific renderer's mesh type |
| License Key | Locally-validated string gating SDK use, no network call |
