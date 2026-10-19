# SomEDA Roadmap

Realistic, milestone-driven plan from proof-of-concept to a commercial-grade open AI-EDA. Each milestone ships working, tested code and a release. Effort estimates assume 1-2 experienced Rust engineers full-time; community contributions accelerate everything from M1 onward.

Maintained by [SomTronics](https://somtronics.org)

---

## Milestone Summary

| Milestone | Version | Theme | Est. effort | Status |
|---|---|---|---|---|
| **M0** | v0.1.0 ✅ | Proof-of-concept core | done | shipped |
| **M1** | v0.2.0 | Verification spine | 4-6 weeks | next |
| **M2** | v0.3.0 | Fabrication-ready board tooling | 6-10 weeks | |
| **M3** | v0.4.0 | Routing & zones | 8-12 weeks | |
| **M4** | v0.5.0 | Schematic + interop | 8-12 weeks | |
| **M5** | v0.6.0 | AI orchestration (optional layer) | 6-8 weeks | |
| **M6** | v1.0.0 | GUI application | 12+ weeks | |

---

## M0 — Proof-of-Concept Core (v0.1.0) ✅ SHIPPED

- [x] S-expression parser with round-trip fidelity (no deps)
- [x] Board model: footprints, pads, nets, Edge.Cuts outline
- [x] DRC v0: pad-outside-outline, pad-center clearance, outline closure
- [x] The honest-done repair loop (done re-verified against board state)
- [x] CLI: `info` / `drc` / `validate` / `repair`
- [x] 13 unit tests; CI on Linux/macOS/Windows; release binaries

**Exit criteria met:** parses real KiCad boards; repair loop demonstrably converges; done-claims rejected on dirty boards.

---

## M1 — Verification Spine (v0.2.0)

The goal: the strongest DRC/ERC core in open source, headless-first.

- [ ] **Full pad geometry DRC**: rectangle-bbox distance (not center-distance); rotation-aware pad shapes (rect, circle, round-rect); clearance vs. copper-to-copper with net-class overrides
- [ ] **Trace/segment DRC**: segment-segment clearance, segment-pad clearance, width minimums per net class
- [ ] **Annular ring & hole checks**: drill vs. annulus per IPC-2222 minima; hole-to-hole; hole-to-edge
- [ ] **Edge clearance**: copper-to-outline per fab min
- [ ] **Net class engine**: named classes with width/clearance; per-net overrides; diff-pair grouping; violation reporting references the class
- [ ] **Schematic model + ERC v0**: symbols, wires, labels; floating-pin detection; missing-power-flag detection; single-net and multi-net audit; `connectivity.verified` re-derivation from wires
- [ ] **Report module**: DRC/ERC output as Markdown + JSON; stable finding codes (`pad-outside-board`, `clearance`, `annular-ring`...) for tooling
- [ ] **Generalized honest-done**: `Verified` trait across DRC + ERC; repair loop drives both; round budget + escalation notes
- [ ] **Fuzz the sexp parser** (`cargo-fuzz`) — format parsing must never panic
- [ ] **Golden-file tests**: corpus of real KiCad boards (our own + public domain designs), validated on every commit

**Exit criteria:** DRC/ERC report on a real, multi-footprint board with correct geometry math (verified against a reference CAM tool); zero panics on a 10M-iteration parser fuzz; repair loop converges on a curated "broken boards" corpus.

---

## M2 — Fabrication-Ready Board Tooling (v0.3.0)

The goal: from netlist to orderable Gerbers, headless.

- [ ] **Netlist extraction**: pin-level connectivity graph from schematic; forward-annotation to board; per-net ratsnest generation
- [ ] **Placement scoring engine**: Manhattan ratsnest cost; courtyard overlap detection; region/group constraints; deterministic "auto-arrange" that minimizes cost (hill-climb, seeded)
- [ ] **Stackup + impedance calculator**: microstrip/stripline/coplanar formulas (public IPC/broadcast literature); width solver for target Z0; layer table with fab presets
- [ ] **Gerber X2 writer**: copper/mask/paste/silk/edge layers; Excellon drill; `.gbrjob`; validated byte-level against `gerbonara` on every commit; round-trip our own output
- [ ] **BOM/CPL export**: per-line BOM with MPN fields; pick-and-place XYRotation
- [ ] **CLI grows**: `someda stackup`, `someda bom`, `someda gerber`, `someda ratsnest`
- [ ] **Net-class-driven DRC** wired end-to-end: width/clearance enforced on traces by class
- [ ] **Docs**: a "from netlist to fab" guide; a real example board carried through every command

**Exit criteria:** a non-trivial open-hardware board (e.g. a 30-part design) goes netlist → placement score → DRC-clean → Gerber package that a real CAM tool accepts and a fab quotes.

---

## M3 — Routing & Zones (v0.4.0)

The goal: deterministic autorouting that finishes what it starts.

- [ ] **Grid router v1**: A*/Dijkstra on a clearance-aware grid; two-layer; per-net class costs; via placement with annulus checks; rip-up-and-reroute loop with bounded iterations; unrouted-net reporting with honest failure causes
- [ ] **Push-and-shove v0**: neighbor displacement for simple cases (traces only)
- [ ] **Zone/pour fill**: polygon pour with island detection, clearance to nets, thermal-relief spokes; GND-pour-on-B.Cu default flow
- [ ] **Router verification**: every routed net re-checked by DRC; router output must pass its own gate before commit (honest-done applied to routing)
- [ ] **Length-tuning stubs**: serpentine placement for diff pairs within budgets (USB3/DDR class rules from public specs)
- [ ] **Benchmarks**: open-source boards corpus; route-completion %, time, and DRC-clean % published per release

**Exit criteria:** ≥80% route completion with zero DRC errors on the benchmark corpus two-layer boards; router never claims a net it didn't verify.

---

## M4 — Schematic + Interop (v0.5.0)

The goal: full KiCad project interop.

- [ ] **Schematic editor core (data + checks, not GUI)**: symbol libraries, multi-sheet hierarchy, annotations, bus/wire algebra; ERC full rule set
- [ ] **KiCad project round-trip**: `.kicad_pro` + schematic + board in/out without semantic loss (byte-level where feasible; diff tool to prove it)
- [ ] **EAGLE importer** (`.sch`/`.brd`, XML): the highest-value interop target first
- [ ] **EasyEDA importer** (JSON): second target
- [ ] **Altium importer**: research spike, then decide scope honestly
- [ ] **Library tooling**: footprint/symbol generation from vendor-neutral land-pattern math (IPC-7351B formulas, public standard) with a two-pass self-check

**Exit criteria:** import an EAGLE project, run full DRC/ERC, and export Gerbers that match a hand-tuned reference within fab tolerances.

---

## M5 — AI Orchestration Layer (v0.6.0) — *optional by design*

The goal: accelerate the core with LLMs, never bypassing it.

- [ ] **Model gateway**: OpenAI-compatible HTTP client + local-runtime endpoint config; no provider names in core; keys via env; fully optional feature flag (`--features ai`)
- [ ] **Orchestrator v1**: stage pipeline as typed, versioned JSON contracts — request classify → plan → apply → verify → summarize; strict schema validation with reject-on-unknown-fields
- [ ] **Agent repair protocol**: bounded rounds (≤N tools + note), done-claims verified against live `Verified::verify()`, honest escalation; the loop the study documented, built on our core
- [ ] **Original orchestration prompts**: our own, MIT, in-repo (none from the study; the study's prompt corpus is not redistributed)
- [ ] **AI conformance suite**: any model must pass a fixed set of JSON-contract tests before it's allowed to drive edits; failures fall back to deterministic-only mode
- [ ] **CLI**: `someda ai plan|repair|explain` — with a `--dry-run` that shows the contract diff before applying

**Exit criteria:** with AI enabled, an end-to-end "describe a circuit → plan → apply → verify → summarize" run on a small board where every applied action passed core verification; with AI disabled, zero feature regression.

---

## M6 — GUI Application (v1.0.0)

The goal: the desktop app.

- [ ] **egui or iced frontend** over the core crate; immediate-mode canvas for board/schematic
- [ ] **Command palette + undo/redo** over core commands; every GUI action routes the same `Verified` gate as CLI
- [ ] **Live DRC/ERC feedback** (incremental re-check on edit)
- [ ] **3D preview**: off-axis board render (custom minimal renderer first; consider `wgpu` later)
- [ ] **Packaging**: signed installers for Windows/macOS/Linux; nightly channel from CI
- [ ] **Docs & onboarding**: guided "first board" walkthrough; every milestone's CLI capability reflected in the GUI

**Exit criteria:** a user completes netlist → placement → route → DRC → Gerber export entirely in the GUI with live verification, on all three OSes.

---

## Effort, Staffing & Honest Expectations

- **M0 → M1 → M2** are achievable by 1-2 strong Rust engineers; M2 end is where the tool becomes genuinely useful for real boards
- **M3 (router)** is the classic hard part — budget the most calendar time and welcome collaboration; there is a reason routers are rare in open source
- **M5** deliberately comes *after* the core is proven: the study's biggest finding is that the commercial product's quality comes from deterministic verification, not from the AI
- **GUI last** (M6): headless-first means the core is scriptable, CI-testable, and embeddable — the GUI then can't rot the logic

**Community leverage:** every milestone's exit criteria are CI-verified, so external contributors can pick a module from the module map in [ARCHITECTURE.md](ARCHITECTURE.md) with a clear definition of done.

---

## Versioning & Release Policy

- Semver; a release per milestone; nightly CI builds
- Every release: unit tests + golden-file corpus + fuzz targets green on Linux/macOS/Windows
- DRC finding codes and JSON contracts are stable within a major version (tooling depends on them)