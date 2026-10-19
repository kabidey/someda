# someda

**From a circuit plan to a fab-ready KiCad project, with every claim checked.**

`someda` is a KiCad-compatible EDA engine in Rust. It takes a small JSON
description of a circuit — parts, nets, intent such as "this connector on the
left edge" — and produces a KiCad schematic and board, placed and routed,
audited for signal integrity, stability and thermals, verified by KiCad's own
ERC/DRC, and exported as Gerbers, drill, position and BOM files plus a
JLCPCB order package and 3D renders. It never says "done" unless six
independent gates agree.

The plan can come from a person, a script, or any language model: the model
writes intent, the engine owns geometry, and the engine repairs what a plan
edit can fix before building again.

[![release](https://img.shields.io/github/v/release/kabidey/someda)](https://github.com/kabidey/someda/releases)
MIT license · maintained by [SomTronics](https://somtronics.org)

## What a build does

```
plan ─▶ schematic ─▶ footprints ─▶ placement ─▶ routing ─▶ audits ─▶ kicad-cli ─▶ exports ─▶ six gates
```

- **Schematic** — a readable sheet (library symbols, labelled pins, project symbol library), judged by kicad-cli ERC.
- **Footprints** — real KiCad library footprints resolved from `Lib:Name`; a misspelled name is replaced by the closest library footprint in a repair round, a part the libraries lack is reported, never guessed.
- **Placement** — courtyard-aware: mounting holes at their corners, the centre part, edge connectors largest first (rotated so the connector's opening faces its edge, courtyard on the edge or protruding by `edge_overhang_mm`), parts next to the pins they share a net with, a keep-out ring for fine-pitch escape vias, hot parts kept apart. A connector that finds no room on its edge is moved to an adjacent edge and the board re-placed.
- **Routing** — 2, 4 or 6 copper layers on JLCPCB rules; copper planes on inner layers with per-pad fanout vias (split-plane islands grow to reach their decoupling capacitors); depth-staggered escape vias for every fine-pitch pin; **differential pairs first** as coupled corridors at a fab-table cross-section with matched lengths; **3W/5W crosstalk spacing enforced** for every fast net; impedance-controlled nets at widths solved from the stackup, confined to plane-referenced layers; rip-up and reroute.
- **Audits on the routed copper** — per-net frequency, stray L/C and SWR, λ/20 stubs, unterminated transmission lines, crosstalk re-measured against the router's own breakout zones, per-capacitor mounting-loop inductance, regulator output capacitance vs datasheet minimums, crystal loads and traces, buck switch/feedback nodes, PDN |Z(f)| per rail, junction temperatures, JLCPCB fab scope.
- **kicad-cli** — ERC, DRC with zones refilled (judged from the report file), Gerbers, drill + map, position file, BOM, 3D renders with component bodies.
- **Exports** — detailed BOM (qty, references, value, MPN, LCSC, package) and a JLCPCB order package (Protel-named Gerber zip, merged Excellon, assembly BOM, CPL with per-package rotation corrections).
- **Six gates** — DRC, shorts, unrouted, pours, Gerbers, drill. All six pass = `HONEST-DONE`; anything else = `NOT DONE`, with the failing gate named.
- **Repair rounds** — up to `SOMEDA_REPAIR_ROUNDS` (default 3): footprint substitution, a 100 nF capacitor where ERC finds an IC rail without one, regulator output capacitance, board growth when parts overlap or connections stay unrouted (only when the plan fixes no size). Every change is a `repair:` line, every edit only the author can make a `suggest:` line with the exact plan edit, and the plan as built is written to `plan.built.json`.

## The desktop app

`someda-app` is the same engine behind a window, for people who do not want a terminal.
Double-click it and it walks you through:

1. **Language model** — pick a provider (Anthropic, OpenAI, DeepSeek, Kimi, Mistral, Z.ai,
   Ollama Cloud, OpenRouter, NVIDIA NIM, a local Ollama, or any OpenAI-compatible endpoint),
   paste a key, list its models, send one test message. The key is stored in your user config
   directory (owner-only) and reaches the engine only through its environment.
2. **KiCad and libraries** — the app finds `kicad-cli` and the footprint, symbol and 3D model
   libraries, or installs KiCad for you (Linux apt, the official Windows installer silently,
   the macOS disk image).
3. **What to build** — you describe the board; the model asks only what changes the design,
   as multiple-choice questions with a recommended answer; then it proposes a plan. The
   engine checks every part against the installed KiCad libraries: symbol, footprint, every
   pin number against the symbol's pin table (shown to you: `U1: 1 GND · 2 TR · 3 Q …`).
   Misspellings are corrected by the engine, a real choice becomes a question with the real
   options, a wrong pin goes back to the model with the pin table. Nothing is built until
   the engine is satisfied.
4. **Build** — footprints and 3D models the libraries lack are fetched from KiCad's own
   repositories, then the engine runs exactly as on the command line; you watch its log.
5. **Result** — the six gates, the 3D render, repairs, suggestions and warnings; buttons
   open the project folder, the board in KiCad and the JLCPCB package.
6. **Change the design** — describe the change; the model proposes a revised plan; you see
   the difference and confirm; the engine builds the next version (every version's plan and
   outputs are kept).

Verified on Linux under a virtual display with the 555 quick start against an
OpenAI-compatible server (all six gates, then an edit and all six gates again). The Anthropic
path implements the published Messages API but was not round-tripped against the live service
in that environment. The Windows and macOS binaries are built by the same release workflow;
their KiCad install flows have not been executed on those systems yet.

## Install

Prebuilt binaries for Linux x86_64, macOS (Apple silicon and Intel) and Windows x86_64 are on the
[releases page](https://github.com/kabidey/someda/releases): each archive holds `someda` (the
command line) and `someda-app` (the window). Or build from source with stable Rust:

```bash
cargo build --release                               # target/release/someda (three direct dependencies)
cargo build --release --features app --bin someda-app   # the window (eframe/egui)
```

For the verification and export stages you need [KiCad](https://www.kicad.org/) with `kicad-cli`
on your PATH (developed and verified against KiCad 10.0.6; set `SOMEDA_KICAD_CLI` to point at it otherwise)
and KiCad's footprint and 3D model libraries (`kicad-footprints`, `kicad-packages3d` on Debian).
Without `kicad-cli` the engine still plans, places, routes and audits, and reports the skipped
stages as such.

Using it from [Claude Code](https://claude.com/claude-code)? Install the
[someda plugin](https://github.com/kabidey/someda-claude-plugin): it writes the plan from a
conversation, fetches missing footprints and 3D models (KiCad GitLab, LCSC/EasyEDA, vendor
zips), runs this engine, and reports the gates.

## Quick start

`examples/plan_555.json` is an NE555 astable blinking an LED, with the supply on a header at the
left edge and the 555's decoupling capacitor asked to sit next to it:

```json
{
  "name": "555 LED blinker",
  "description": "NE555 astable at about 1 Hz driving an LED",
  "layers": 2,
  "components": [
    {"reference": "U1", "value": "NE555", "lib_id": "Timer:NE555", "footprint": "Package_DIP:DIP-8_W7.62mm", "position": "center"},
    {"reference": "R1", "value": "10k",   "lib_id": "Device:R",   "footprint": "Resistor_SMD:R_0603_1608Metric"},
    {"reference": "R2", "value": "100k",  "lib_id": "Device:R",   "footprint": "Resistor_SMD:R_0603_1608Metric"},
    {"reference": "R3", "value": "330",   "lib_id": "Device:R",   "footprint": "Resistor_SMD:R_0603_1608Metric"},
    {"reference": "C1", "value": "4.7uF", "lib_id": "Device:C",   "footprint": "Capacitor_SMD:C_0805_2012Metric"},
    {"reference": "C2", "value": "100nF", "lib_id": "Device:C",   "footprint": "Capacitor_SMD:C_0603_1608Metric", "near": "U1"},
    {"reference": "D1", "value": "red",   "lib_id": "Device:LED", "footprint": "LED_SMD:LED_0805_2012Metric"},
    {"reference": "J1", "value": "5V in", "lib_id": "Connector:Conn_01x02", "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical", "position": "left_edge"}
  ],
  "nets": [
    {"name": "VCC", "pins": ["J1:1", "U1:8", "U1:4", "R1:1", "C2:1"]},
    {"name": "GND", "pins": ["J1:2", "U1:1", "C1:2", "C2:2", "D1:1"]},
    {"name": "DIS", "pins": ["U1:7", "R1:2", "R2:1"]},
    {"name": "THR", "pins": ["U1:6", "U1:2", "R2:2", "C1:1"]},
    {"name": "OUT", "pins": ["U1:3", "R3:1"]},
    {"name": "LED", "pins": ["R3:2", "D1:2"]}
  ]
}
```

```bash
SOMEDA_PLAN_FILE=examples/plan_555.json SOMEDA_PROJECT_DIR=out someda ai build "555 LED blinker"
```

The build prints every stage and ends with the gate table. With someda 0.7.0 and kicad-cli
10.0.6 this plan places 8 parts on a 43 × 35 mm board, routes 15 of 15 connections, passes
kicad-cli ERC and DRC with 0 errors, and reaches all six gates. `out/` then holds:

```
out/circuit.kicad_sch  circuit.kicad_pcb  circuit.kicad_pro   the KiCad project
out/circuit-erc.rpt    circuit-drc.rpt                         kicad-cli reports
out/fab/               Gerbers, drill + map, positions.csv, bom.csv, bom-detailed.csv, bom.md
out/fab/jlcpcb/        jlcpcb-gerbers.zip, jlcpcb-bom.csv, jlcpcb-cpl.csv
out/render/            board-top.png, board-bottom.png, board-iso.png
out/plan.built.json    the plan as built (rotations, repairs, grown regions)
```

The plan can also come from a language model: with `SOMEDA_API_KEY`, `SOMEDA_BASE_URL` and
`SOMEDA_MODEL` set (any OpenAI-compatible `/chat/completions` endpoint: OpenAI, Azure, vLLM,
Ollama, LM Studio, llama.cpp), `someda ai build "<description>"` asks the model for the plan
and continues exactly as above. `someda ai plan "<description>"` prints the plan only.

## The plan

A plan is intent, never coordinates. The full reference lives in the plugin's
[CONTRACT.md](https://github.com/kabidey/someda-claude-plugin/blob/master/skills/someda/references/CONTRACT.md);
the fields are:

| Field | Meaning |
|---|---|
| `components[].reference`, `value`, `lib_id`, `footprint` | Designator, value (may carry power: `"1W"`), KiCad symbol, KiCad footprint `Lib:Name` |
| `components[].position` | `top_left` … `bottom_right`, `center`, or an edge: `left_edge`, `right_edge`, `top_edge`, `bottom_edge` |
| `components[].edge_at`, `edge_overhang_mm` | Slot along the edge (0 … 1); how far the body front protrudes past the edge (USB-C shells) |
| `components[].near` | Sit next to this reference, on the side of the pins they share |
| `components[].rotation` | Optional; edge connectors need none, the engine turns the opening toward the edge |
| `components[].lcsc`, `mpn` | Part numbers for the BOM, the JLCPCB package and library fetching |
| `nets[].name`, `pins` | Net name and `"REF:PIN"` members |
| `nets[].freq_hz`, `role`, `termination` | Highest frequency; `clock` / `highspeed` / `rf` / `power`; `series` / `parallel` / `on-chip` |
| `layers`, `board` | 2, 4 or 6 copper layers; optional fixed `{width_mm, height_mm}` (otherwise budgeted from the courtyards) |
| `planes[]` | `{net, layer, region, priority}` — whole-layer, a rectangle, or `"auto"` (the owning IC's pins plus 5 mm) |
| `pairs[]` | `{p, n, z_diff, skew_mm}` — differential pairs routed as coupled pairs |

## What has been verified

Every number below comes from an unedited build log; the SBC's complete outputs are kept as
evidence in the plugin repository
([examples/sompi-v3s/evidence](https://github.com/kabidey/someda-claude-plugin/tree/master/examples/sompi-v3s/evidence)).

- **555 blinker** (above): 15 of 15 routed, DRC 0 errors, six gates. The same plan with a misspelled footprint and no decoupling capacitor reaches the six gates after one repair round.
- **6-layer LQFP-48 board**, 26 parts: 31 connections and 46 plane fanouts, DRC 0 errors, six gates.
- **SomPi-V3s**, an Allwinner V3s single-board computer: 132 parts, 90 × 60 mm, six layers (JLC06161H-3313), USB-C, 10/100 Ethernet, microSD, SPI NOR, 40-pin LCD and 24-pin MIPI camera FPCs, audio jack, GPIO header, two bucks and two LDOs. 187 of 187 connections routed, 225 plane fanouts, 104 escape vias; seven differential pairs coupled within their skew budgets (USB 90 Ω: −0.65 mm and +0.52 mm of 1.1, one through a via crossover; Ethernet 100 Ω: ≤ 0.01 mm of 0.25; MIPI 100 Ω: 0.00 mm of 0.15); 0 crosstalk violations; kicad-cli DRC 0 errors; six gates. The engine rotated every edge connector from its footprint, moved the audio jack to the bottom edge when the top edge had no 18 mm slot left, and grew the +1V8 island to reach its capacitors. Its warnings are real and listed: crystal traces of 11–17 mm, a camera 1.8 V capacitor with a 7.4 nH loop, three capacitors beyond the island, board density 147 %.

What "six gates" does not mean: the boards are not simulated and have not been built. Connector
pinouts follow conventions that must be checked against the parts you buy; the 3D bodies fetched
from LCSC are not verified against physical parts; clock-to-data skew across pairs is not
enforced; everything is placed on the top side.

## Environment

| Variable | Effect |
|---|---|
| `SOMEDA_PLAN_FILE` | Build this plan instead of asking a model |
| `SOMEDA_PROJECT_DIR` | Output directory (default `./someda-build`) |
| `SOMEDA_FOOTPRINT_DIRS` | Extra `.pretty` directories, colon-separated, searched before KiCad's |
| `SOMEDA_REPAIR_ROUNDS` | Repair rounds after the first build (default 3; 0 disables) |
| `SOMEDA_STOP_AFTER` | `schematic` or `route`: write that far and stop (review a schematic before fetching libraries) |
| `SOMEDA_KICAD_CLI` | Path to `kicad-cli` |
| `SOMEDA_API_KEY`, `SOMEDA_BASE_URL`, `SOMEDA_MODEL` | Planner endpoint (also read as `OPENAI_*`) |
| `SOMEDA_ROUTE_DEBUG` (`1`/`2`), `SOMEDA_ROUTE_DEBUG_NET`, `SOMEDA_PLACE_DEBUG` | Router and placer diagnostics: occupancy maps around failed pins, corridor decisions, placement targets |
| `KICAD10_3DMODEL_DIR` (or 9/8) | Where the 3D bodies for the renders live, when KiCad is not in a standard place |

## Board tools

The engine also works on existing KiCad files:

```
someda info       <board.kicad_pcb>          summary of the board
someda drc        <board.kicad_pcb>          design-rule checks
someda validate   <board.kicad_pcb>          parse + DRC (exit 1 on errors)
someda repair     <board.kicad_pcb>          nudge pads inside the outline until DRC is clean
someda report     <board.kicad_pcb> [json|md]
someda erc        <schematic.kicad_sch>      electrical rule check
someda netclasses <board.kicad_pcb>          net class summary
```

## Repository

```
src/orchestrator.rs   the pipeline: plan → stages → gates
src/repair.rs         repair rounds and plan edits
src/placer.rs         courtyard placement
src/router.rs         grid router; router/pairs.rs: differential-pair corridors
src/si.rs stability.rs pdn.rs thermal.rs   audits on the routed board
src/formulas.rs materials.rs netfreq.rs    physics, JLCPCB stackups, per-net frequency
src/footprint_lib.rs board.rs sexp.rs      KiCad footprints, board model, s-expressions
src/schematic.rs drc.rs rules.rs           schematic generation + ERC, engine DRC, fab rules
src/kicad_cli.rs bom.rs jlcpcb.rs          kicad-cli stages, BOM, JLCPCB package
src/app/              the desktop app's logic (providers, config, dependencies, symbols, interview, project, runner)
src/bin/someda-app/   the window (egui), feature "app"
examples/             plan_555.json, formula_probe (prints every formula), app_e2e (the app's flow headless), pipeline examples
tests/                corpus of real boards and fixtures
```

`cargo test` runs the suite (about 200 tests); `cargo run --example formula_probe` prints the
physics outputs so they can be compared with independent references. Pushing a `v*` tag builds
the release binaries.

[ARCHITECTURE.md](ARCHITECTURE.md) explains the design and its honest-done invariant,
[ROADMAP.md](ROADMAP.md) the milestones, [CHANGELOG.md](CHANGELOG.md) what each version
changed and what it was verified against.

## Provenance

someda is an original implementation of the architecture patterns documented in the
[SomTronics AI-EDA study](https://github.com/kabidey/somtronics-ai-eda-study) of a commercial
AI-EDA product: public KiCad file formats, clean-room DRC rules and repair-loop contract. It
contains no code, text or material from the analysed product. MIT license.
