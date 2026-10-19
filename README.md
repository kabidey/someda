# someda

**A clean-room KiCad-compatible EDA core in Rust.**

SomTronics ([somtronics.org](https://somtronics.org)) · MIT license

`someda` implements the architecture patterns documented in the
[SomTronics AI-EDA Study](https://github.com/kabidey/somtronics-ai-eda-study)
as original, working code:

- **S-expression parser** for KiCad board/schematic files
- **Board model**: footprints, pads, nets, and the Edge.Cuts outline
- **DRC engine**: pad-outside-board, pad clearance, outline closure
- **The honest-done repair loop**: done claims are re-verified against the
  board's own state — claiming done with open work is rejected

## Usage

```
someda info    <board.kicad_pcb>     summary of the board
someda drc     <board.kicad_pcb>     run design-rule checks
someda validate <board.kicad_pcb>    parse + DRC (exit 1 on errors)
someda repair  <board.kicad_pcb>     run the honest-done repair loop
someda report  <board.kicad_pcb> [json|md]  DRC report
someda erc     <schematic.kicad_sch> electrical rule check
someda netclasses <board.kicad_pcb>  net class summary
someda ai plan <description>         plan a circuit with an LLM (print JSON)
someda ai build <description>        plan + generate + verify + export (full project)
```

### AI Mode (optional — works with any OpenAI-compatible API)

```bash
export SOMEDA_API_KEY="sk-..."   # or OPENAI_API_KEY
export SOMEDA_BASE_URL="https://api.openai.com/v1"  # or any compatible API
export SOMEDA_MODEL="gpt-4o-mini"  # or any model

someda ai build "Build a 555 timer LED blinker with 1Hz output"
```

The LLM plans the circuit as a strict JSON contract (components + nets);
someda's deterministic generators write `.kicad_sch` + `.kicad_pcb`; kicad-cli
runs ERC/DRC and exports Gerbers, drill, position, and BOM. The honest-done
loop verifies every stage.

Works with: OpenAI, Azure OpenAI, vLLM, Ollama (`/v1` mode), LM Studio,
llama.cpp server — any OpenAI-compatible `/chat/completions` endpoint.

## Example

```
$ someda validate board.kicad_pcb
parse: OK
DRC: 0 error(s), 0 warning(s)
result: CLEAN — board is parseable and passes all checks

$ someda repair broken.kicad_pcb
repair loop: inspecting board...
DRC: 1 error(s), 0 warning(s)
  [ERROR] pad-outside-board: Pad R1:1 has copper outside the board outline. ...
round 1: nudging 1 footprint(s) into outline: R1
DONE ACCEPTED: repair attempted; verify
rounds used: 2
DRC: 0 error(s), 0 warning(s)
```

## Build

```
cargo build --release    # zero dependencies
cargo test               # 13 tests
```

## What it is / is not

This is an **original implementation** of the concepts from our architecture
study of a commercial AI-EDA product: the file formats are public KiCad
s-expression syntax; the DRC rules and repair-loop contract are clean-room
implementations of the design patterns we documented. It contains no code,
text, or material from the analyzed product.

## v0.4.0 — The Full Idea-to-PCB Pipeline (PCB-Superpower Skills 101-103)

The application now walks a circuit from **idea to fab-ready PCB** in one pipeline,
implementing the [PCB-Superpower](https://github.com/kabidey/PCB-Superpower)
orchestration layer (skills 101-103) as deterministic Rust:

| Skill | someda module | What it does |
|-------|--------------|--------------|
| 102 Placement Handoff | `placer.rs` | The deterministic solver that owns geometry: 9-region semantic positions, rows, groups, apart (2D distance), center-out ring packing ordered by ratsnest centrality. The LLM never emits coordinates. |
| 101 SomRoute Autorouting | `router.rs` | A* grid router with the skill's verified defaults: 0.25 mm trace, 0.2 mm clearance, 0.6/0.3 vias, 0.25 mm edge clearance, Manhattan-only moves, full segment rasterization, via halos, honest `routed N of M` reporting. |
| 103 DRC Repair Loop | `bringup.rs` + orchestrator stage 7 | The 6-gate export preflight (DRC, shorts, unrouted, pours, Gerbers, drill) as the honest-done contract: all six pass = orderable, anything else = NOT DONE. |
| All 103 skills | `formulas.rs` | Every quantitative formula: IPC-2152 current, microstrip Z0 + width solver, λ/20 via stubs, critical length, 3W/5W crosstalk, shielding A/R losses, slot leakage, ground bounce, via loop inductance, thermal chain (θJA, Tj, via grids), PDN targets, cap SRF, L-match RF networks, area budgets, USB3/DDR/PCIe length budgets — each verified against the skills' own worked examples. |
| Plan schema | `boardplan.rs` | The 7-form contract: plan / bringup phases (layers → stackup → net classes → placement → planes → route → checks), route/unroute flags with net scoping, strict validation. |

### Live-verified end to end

```
someda ai build "555 LED blinker"
  Stage 1  plan (LLM JSON contract)
  Stage 2b placement (deterministic solver, 5 footprints)
  Stage 2c route: routed 303 of 303 connections (0 unrouted)
  Stage 6  someda DRC: 0 errors
  Stage 7  export preflight: drc PASS shorts PASS unrouted PASS pours PASS gerbers PASS drill PASS

kicad-cli pcb drc circuit.kicad_pcb
  Found 0 unconnected items        (was 4 before routing)
  0 electrical violations          (only missing-lib env warnings)
kicad-cli pcb export gerbers → fab/circuit-F_Cu.gtl (routed copper), .drl, .gbrjob
```

KiCad's own DRC drove two router fixes (grid must encode fab geometry; Manhattan-only
moves) — the honest-done loop working exactly as the skills demand.

## v0.5.0 — The Physics-Aware Design Harness

The design now carries physics end to end, per the harness directive:

| Module | What it does |
|--------|--------------|
| `netfreq.rs` | Every NET knows its frequency: declared in the plan (freq_hz/role), derived from component specs ("2.4GHz", "NE555", "74HC00"), or classified by name (power=DC, clock, RF, high-speed). Transmission-line and 3W/5W decisions read this. |
| `materials.rs` | JLCPCB's actual materials and scope (fetched 2026-10-19): FR-4 Er 4.5, 7628/3313/2116 prepregs, soldermask Er 3.8, 4/4mil min, 0.15/0.25 vias, ±10% impedance — the fab contract every design is checked against. |
| `si.rs` | Microwave stability on the ROUTED board: stray L and C measured per net from actual geometry (trace L/C per mm, 0.5nH vias, pad C), SWR from Z0 mismatch vs target, λ/20 stub resonance, net LC self-resonance (oscillation risk), crosstalk parallel-run analysis. |
| `thermal.rs` | Power dissipation parsed per component; θJA package classes; junction temps vs limits; thermal via budgets; copper balance; thermal-aware area claims. |
| Router upgrades | Impedance-controlled nets route at width SOLVED from the microstrip equation on the JLCPCB stackup (50Ω → 1.44mm on 1.6mm FR-4); impedance nets route first; neck-down near fine-pitch pads; width-aware pad clearance; stackup counsel when the solved width says "use a thinner dielectric". |

Live-verified with an RF fixture (2.4GHz path + 555): the harness flagged the
naive board's SWR 2.16 at default width, then the router solved the RF trace
to 50Ω and KiCad DRC passed clean. The honest-done loop applied to physics.

## Claude Code Plugin — PCB from a vibe

Use someda directly inside Claude Code:

```bash
claude plugin marketplace add kabidey/someda-claude-plugin
claude plugin install someda@someda
```

Then just describe a circuit in conversation:

```
/pcb-build a 555 timer blinking an LED at 1Hz with proper decoupling
```

Claude plans the circuit (real pinouts, per-net frequencies, per-component
power); the compiled someda engine places, routes with physics, audits
against JLCPCB's real materials, verifies through kicad-cli, and exports
Gerbers — gated by the honest-done contract.
