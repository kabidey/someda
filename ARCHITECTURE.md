# SomEDA Architecture

**A commercial-grade, open-source AI-EDA application in Rust.**

This architecture is derived from our [study of a commercial AI-EDA product](https://github.com/kabidey/somtronics-ai-eda-study): we use its *architectural findings* (module responsibilities, pipeline stages, data contracts, the honest-done invariant) as requirements. All code implementing this architecture is original. No vendor code, text, or prompts are used.

Maintained by [SomTronics](https://somtronics.org) · MIT

---

## The Core Principle (from the study)

> **LLMs plan; deterministic code acts.**

Every AI stage emits a strict JSON contract. A deterministic Rust engine consumes it, owns all geometry, placement, and file syntax, and verifies the result. The LLM is never trusted with coordinates, pad dimensions, or file text.

The second invariant, enforced everywhere:

> **Done is a fact the board states, not a claim an agent makes.**

Every "done" from an AI or a user tool is re-verified against live design state. Claiming done with open work is rejected. This is `someda`'s `RepairLoop` generalized to the whole application.

---

## System Overview

```
                        ┌────────────────────────────────────┐
                        │            someda-gui              │
                        │  (egui/iced frontend, later phase)  │
                        └───────────────┬────────────────────┘
                                        │ commands
                        ┌───────────────▼────────────────────┐
                        │          someda-core (lib)          │
                        │                                    │
                        │  ┌──────────┐  ┌──────────────┐    │
                        │  │ sexp     │  │ board model  │    │
                        │  │ (parser) │  │ (netlist/geom)│   │
                        │  └──────────┘  └──────────────┘    │
                        │  ┌──────────┐  ┌──────────────┐    │
                        │  │ drc      │  │ erc          │    │
                        │  │ engine   │  │ engine       │    │
                        │  └──────────┘  └──────────────┘    │
                        │  ┌──────────┐  ┌──────────────┐    │
                        │  │ router   │  │ repair loop  │    │
                        │  │ (later)  │  │ (honest-done)│    │
                        │  └──────────┘  └──────────────┘    │
                        │  ┌──────────┐  ┌──────────────┐    │
                        │  │ netclass │  │ exporter     │    │
                        │  │ engine   │  │ (gerber)     │    │
                        │  └──────────┘  └──────────────┘    │
                        └───────────────┬────────────────────┘
                                        │ JSON contracts
                        ┌───────────────▼────────────────────┐
                        │        someda-orchestrator           │
                        │  AI pipeline stages (M5+):          │
                        │  request-router → planner →         │
                        │  edit-compiler → repair → summary   │
                        └───────────────┬────────────────────┘
                                        │ provider-agnostic HTTP
                        ┌───────────────▼────────────────────┐
                        │      model gateway (M5+, opt-in)    │
                        │  cloud APIs or local runtimes;      │
                        │  the app is fully usable without it  │
                        └────────────────────────────────────┘
```

**Key design rule:** the deterministic core is the product. The AI layer is an *optional accelerator* that must pass the same verification gates as a human. The app works 100% offline with zero AI.

---

## Module Map (derived from the study's service map)

The studied commercial app distills to ~549 internal service files. We group those responsibilities into the following Rust modules. Each maps to a `someda-core` module with its own tests and exit criteria.

### Layer 1: File formats (the foundation)

| Module | Responsibility (from study findings) | Status |
|---|---|---|
| `sexp` | S-expression lexer/parser/printer with round-trip fidelity; typed accessors | ✅ shipped (M0) |
| `board` | KiCad board model: footprints, pads, nets, outlines, zones, net classes | ✅ shipped (M0); expand in M1 |
| `schematic` | KiCad schematic model: symbols, wires, labels, power symbols, instances | M1 |
| `importers` | Altium/EAGLE/EasyEDA import (each is its own project; do EAGLE first) | M4+ |

### Layer 2: Verification engines (the product's spine)

| Module | Responsibility | Status |
|---|---|---|
| `drc` | Geometric rule checks: clearance (with real pad/trace geometry), outline closure, annular ring, hole clearances, zone fill conflicts, edge clearances | ✅ M0: outline + pad-center checks; M1: full pad bbox geometry |
| `erc` | Electrical rules: floating pins, unconnected nets, missing power symbols, shorted outputs, no-connect audits | M1 |
| `netclass` | Net class contracts: width/clearance per net, differential pairs, priority | M1 |
| `netlist` | Netlist extraction and connectivity re-derivation from schematic (the study's "connectivity.verified" concept) | M1–M2 |
| `repair` | The honest-done loop: bounded rounds, inspect-first, done re-verification | ✅ shipped (M0); generalize in M1 |

### Layer 3: Geometry engines (hard, do incrementally)

| Module | Responsibility | Target |
|---|---|---|
| `placement` | Deterministic placement scoring: ratsnest minimization, courtyard overlap, region grouping | M2 |
| `router` | Maze/A* router on a grid with clearance costs; rip-up; diff-pair length tuning later | M3 (v0.3) |
| `zone` | Polygon pour fill with clearance islands and thermal reliefs | M3 |
| `stackup` | Layer stackup + impedance calculator (microstrip/stripline formulas from the public IPC literature) | M2 |

### Layer 4: Output

| Module | Responsibility | Target |
|---|---|---|
| `gerber` | Gerber X2 writer + Excellon drill + job file (validated with `gerbonara`) | M2 |
| `bom` | BOM export + CPL pick-and-place | M2 |
| `report` | DRC/ERC reports in Markdown/JSON (machine + human readable) | M1 |

### Layer 5: AI orchestration (only after core is proven)

| Module | Responsibility | Target |
|---|---|---|
| `orchestrator` | Stage pipeline: request classify → plan → JSON contract → deterministic apply → verify → summarize. Each stage is a typed struct, versioned (`{"v":1,...}`) | M5 (v0.5) |
| `gateway` | Provider-agnostic model calls (OpenAI-compatible HTTP + local runtimes); never hard-codes provider names into core | M5 |
| `agentloop` | The repair-agent protocol: rounds of ≤N tools + note; done-claims verified against live board state (extends `repair`) | M5 |

---

## The Honest-Done Contract (application-wide invariant)

Every mutating subsystem implements this trait:

```rust
pub trait Verified {
    type Report;
    fn verify(&self) -> Self::Report;     // run against live state
    fn is_done(&self, r: &Self::Report) -> bool; // the state decides, not the caller
}
```

- AI summaries, CLI commands, and GUI actions all route through it
- A "done" summary is only rendered after `is_done(verify()) == true`
- Verification reports are user-visible artifacts (never hidden)

## Data Contracts

All AI↔core boundaries are strict JSON with versioned schemas (study finding: every commercial stage used a versioned JSON contract):

```json
{ "v": 1, "stage": "place", "actions": [ ... ], "note": "one line shown to user" }
```

Unknown fields: reject. Missing required fields: reject. The core publishes its accepted schema in `docs/contracts/` — AI providers must match it, never the reverse.

## Non-Goals (deliberate)

- **No cloud dependency.** Everything except the optional model gateway works offline.
- **No telemetry.** The study documented the vendor's telemetry stack; we ship none.
- **No proprietary formats out.** KiCad s-expr in/out; Gerber out; no lock-in.
- **No vendored AI prompts from the study.** Our orchestration prompts are original and shipped in-repo under MIT.

---

## Relationship to the Study Repo

- [somtronics-ai-eda-study](https://github.com/kabidey/somtronics-ai-eda-study) — the analysis (what commercial AI-EDA does)
- **someda** (this repo) — the implementation (original code doing it, honestly labeled by milestone)