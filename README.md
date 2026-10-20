# someda

**Describe a circuit. Get a KiCad project you could send to a board house, with every claim checked.**

someda is an electronics design engine written in Rust. You give it a small description of a
circuit: the parts, how they connect, and a bit of intent such as "put the USB connector on
the top edge". It produces a KiCad schematic and board, placed and routed, checked for signal
integrity, stability and heat, verified by KiCad's own rule checks, and exported as Gerbers,
drill files, a bill of materials, a JLCPCB order package and 3D renders.

It never tells you a board is done unless six independent checks agree. When something is
wrong, it tells you what, in plain words, instead of hiding it.

The description can come from you, from a script, or from a language model. The model only
writes intent. The engine owns every coordinate, every trace and every verdict. If a plan has
a problem the engine can fix on its own (a misspelled footprint, a missing decoupling
capacitor), it fixes it and builds again.

[![release](https://img.shields.io/github/v/release/kabidey/someda)](https://github.com/kabidey/someda/releases)
MIT license. Made by [SomTronics](https://somtronics.org).

## Two ways to use it

**The desktop app.** `someda-app` is a window. Double-click it and it guides you from "which
language model" to "here are your Gerbers". This is the easiest way in. See
[The desktop app](#the-desktop-app) below.

**The command line.** `someda` is the engine itself. Write a plan file, run one command, read
the result. Scripts, CI pipelines and other programs use this. See [Quick start](#quick-start).

Both come in every release archive.

## What happens in a build

```
plan -> schematic -> footprints -> placement -> routing -> audits -> KiCad checks -> exports -> six gates
```

1. **Schematic.** A readable sheet with real library symbols and labelled pins. KiCad's ERC
   checks it.
2. **Footprints.** Real KiCad library footprints, found by their library name. A misspelled
   name is replaced by the closest footprint in that library, and the engine says so. A part
   the libraries do not have is reported, never guessed.
3. **Placement.** Mounting holes go in their corners. Edge connectors are turned so their
   opening faces the edge, then placed on it. Parts sit next to the pins they share a net
   with. Fine-pitch chips get room for their escape vias. Hot parts keep their distance.
   A connector that finds no room on its edge is moved to the next edge and the board is
   placed again.
4. **Routing.** Two, four or six copper layers on JLCPCB rules. Copper planes on the inner
   layers, with a via for every pad that needs them. Escape vias for every fine-pitch pin.
   Differential pairs are routed first, as coupled pairs at a real cross-section from the
   fab's impedance tables, with matched lengths. Fast nets keep 3W/5W spacing from each
   other. Impedance-controlled traces get the width the stackup calls for and stay over a
   solid plane. When something does not fit, the router rips up and tries again.
5. **Audits on the routed copper.** Stray inductance and capacitance per net. Standing-wave
   ratio and stub resonances. Unterminated transmission lines. Crosstalk measured on the
   actual copper. The loop inductance of every decoupling capacitor as mounted. Regulator
   output capacitance against the datasheet. Crystal loads and trace lengths. Switch-node
   loops. Power delivery impedance per rail. Junction temperatures. Whether JLCPCB can make
   it at all.
6. **KiCad.** ERC, DRC with the zones refilled, Gerbers, drill files and map, position file,
   bill of materials, 3D renders with component bodies.
7. **Exports.** A detailed BOM (quantity, references, value, part number, LCSC number,
   package) and a JLCPCB order package: Gerber zip with Protel names, merged drill file,
   assembly BOM, and a placement file with the rotation corrections JLCPCB expects.
8. **Six gates.** DRC, shorts, unrouted connections, copper pours, Gerbers, drill. All six
   pass and the build says `HONEST-DONE`. Anything else is `NOT DONE`, with the failing gate
   named.

If the plan had something the engine could repair, it repairs it and runs again, up to three
times by default. Each change is printed as a `repair:` line. Anything only you can change
is printed as a `suggest:` line with the exact edit. The plan as it was finally built is
saved next to the outputs as `plan.built.json`.

## The desktop app

Start `someda-app`. It takes you through six screens.

**Language model.** Pick a provider: Anthropic, OpenAI, DeepSeek, Kimi, Mistral, Z.ai,
Ollama Cloud, OpenRouter, NVIDIA NIM, an Ollama running on your own machine, or any other
endpoint that speaks the OpenAI protocol. Paste a key, ask the provider for its model list,
send one test message to be sure it works. The key is saved in your user configuration
folder where only you can read it, and it is handed to the engine through the environment,
never on a command line and never in a log.

**KiCad and libraries.** The app looks for `kicad-cli` and KiCad's footprint, symbol and
3D model libraries. If they are missing it can install KiCad for you: with apt on Debian
and Ubuntu, with the official installer on Windows, from the disk image on macOS.

**What to build.** Say what the board is for, what powers it, what it connects to. The
model may only do two things: ask you a few multiple-choice questions, each with a
recommended answer, about things that change the design; or propose a plan. It does not get
to ask about pinouts, and it does not get to declare the plan finished. The engine does
that. It checks every part against the installed KiCad libraries: does the symbol exist,
does the footprint exist, is every pin number the plan uses really a pin of that symbol.
It shows you the pin table it found, for example `U1: 1 GND, 2 TRIG, 3 OUT, 7 DISCH, 8 VCC`.
A misspelled name is corrected on the spot. When there is a real choice, you get a question
with the real options. When the model used a pin that does not exist, the plan goes back to
the model together with the pin table, and the model tries again. Nothing is built until the
engine is satisfied.

**Build.** Footprints and 3D models the libraries lack are fetched from KiCad's own
repositories. Then the engine runs exactly as it does on the command line, and you watch its
log.

**Result.** The six gates, the 3D render, what the engine repaired, what it suggests, and
the warnings it wants you to read. Buttons open the project folder, the board in KiCad and
the JLCPCB package.

**Change the design.** Describe the change in a sentence. The model proposes a revised plan.
You see the difference, line by line, and confirm. The engine builds the next version. Every
version keeps its plan, its outputs and its build log, so nothing is ever lost.

What has been verified: the whole flow on Linux under a virtual display with the 555
quick-start circuit, against an OpenAI-compatible server, through two versions, all six gates
both times. The Anthropic connection follows the published Messages API but was not tried
against the live service in that environment. The Windows and macOS binaries are built by the
same release workflow; their KiCad install flows have not yet been run on those systems.

## Install

Download the archive for your platform from the
[releases page](https://github.com/kabidey/someda/releases). Each one holds `someda` (the
command line) and `someda-app` (the window). Linux x86_64, macOS on Apple silicon and Intel,
and Windows x86_64 are covered.

To build from source you need stable Rust:

```bash
cargo build --release                                    # the command line, three direct dependencies
cargo build --release --features app --bin someda-app    # the window
```

For the verification and export steps you need [KiCad](https://www.kicad.org/) with
`kicad-cli` on your PATH, along with KiCad's footprint and 3D model libraries. Everything was
developed and verified against KiCad 10.0.6. If `kicad-cli` lives somewhere unusual, point
`SOMEDA_KICAD_CLI` at it. Without KiCad the engine still plans, places, routes and audits; it
simply reports the KiCad steps as skipped.

Using [Claude Code](https://claude.com/claude-code)? The
[someda plugin](https://github.com/kabidey/someda-claude-plugin) writes the plan from a
conversation, fetches missing parts, runs this engine and reports the gates.

## Quick start

`examples/plan_555.json` describes an NE555 blinking an LED, with 5 V arriving on a header at
the left edge and the timer's decoupling capacitor asked to sit next to it:

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

Build it:

```bash
SOMEDA_PLAN_FILE=examples/plan_555.json SOMEDA_PROJECT_DIR=out someda ai build "555 LED blinker"
```

The engine prints every stage and finishes with the gate table. With someda 0.8.0 and
kicad-cli 10.0.6 this plan places 8 parts on a 43 by 35 mm board, routes all 15
connections, passes KiCad's ERC and DRC with no errors, and clears all six gates. Afterwards
`out/` contains:

```
out/circuit.kicad_sch  circuit.kicad_pcb  circuit.kicad_pro   the KiCad project
out/circuit-erc.rpt    circuit-drc.rpt                         KiCad's reports
out/fab/               Gerbers, drill and map, positions.csv, bom.csv, bom-detailed.csv, bom.md
out/fab/jlcpcb/        jlcpcb-gerbers.zip, jlcpcb-bom.csv, jlcpcb-cpl.csv
out/render/            board-top.png, board-bottom.png, board-iso.png
out/plan.built.json    the plan as it was built, repairs included
```

You can also let a model write the plan from the command line. Set `SOMEDA_API_KEY`,
`SOMEDA_BASE_URL` and `SOMEDA_MODEL` for any endpoint that speaks the OpenAI protocol
(OpenAI, Azure, vLLM, Ollama, LM Studio, llama.cpp and others), then run
`someda ai build "<what you want>"`. `someda ai plan "<what you want>"` prints the plan and
stops.

## What goes in a plan

A plan is intent, never coordinates. The complete reference is the plugin's
[CONTRACT.md](https://github.com/kabidey/someda-claude-plugin/blob/master/skills/someda/references/CONTRACT.md).
In short:

| Field | Meaning |
|---|---|
| `components[].reference`, `value`, `lib_id`, `footprint` | Designator, value (it may carry power, such as `"1W"`), KiCad symbol, KiCad footprint as `Lib:Name` |
| `components[].position` | `top_left` through `bottom_right`, `center`, or an edge: `left_edge`, `right_edge`, `top_edge`, `bottom_edge` |
| `components[].edge_at`, `edge_overhang_mm` | Where along the edge (0 to 1); how far the body front may stick out past the edge, for USB-C shells and the like |
| `components[].near` | Sit next to this part, on the side of the pins they share |
| `components[].rotation` | Optional. Edge connectors need none; the engine turns the opening toward the edge |
| `components[].lcsc`, `mpn` | Part numbers for the BOM, the JLCPCB package and library fetching |
| `nets[].name`, `pins` | Net name and its members as `"REF:PIN"` |
| `nets[].freq_hz`, `role`, `termination` | Highest frequency; `clock`, `highspeed`, `rf`, `power`; `series`, `parallel`, `on-chip` |
| `layers`, `board` | 2, 4 or 6 copper layers; an optional fixed size in mm (otherwise the engine budgets one from the courtyards) |
| `planes[]` | `{net, layer, region, priority}`: a whole layer, a rectangle, or `"auto"` (the owning chip's pins plus 5 mm) |
| `pairs[]` | `{p, n, z_diff, skew_mm}`: differential pairs, routed as coupled pairs |

## What has actually been built with it

Every number here comes from an unedited build log. The complete outputs of the biggest board
are kept as evidence in the plugin repository:
[examples/sompi-v3s/evidence](https://github.com/kabidey/someda-claude-plugin/tree/master/examples/sompi-v3s/evidence).

- **The 555 blinker** above: 15 of 15 connections routed, DRC clean, six gates. The same plan
  with a misspelled footprint and no decoupling capacitor passes the six gates after one
  repair round, in which the engine corrected the name and added a 100 nF capacitor.
- **A 6-layer board around an LQFP-48**, 26 parts: 31 connections and 46 plane fanouts,
  DRC clean, six gates.
- **SomPi-V3s**, a single-board computer around the Allwinner V3s: 132 parts on 90 by 60 mm,
  six layers on JLCPCB's JLC06161H-3313 stackup, USB-C, 10/100 Ethernet, microSD, SPI flash,
  40-pin LCD and 24-pin MIPI camera connectors, an audio jack, a GPIO header, two bucks and
  two LDOs. 187 of 187 connections routed, 225 plane fanouts, 104 escape vias. All seven
  differential pairs coupled within their skew budgets: the two USB pairs at 0.65 mm and
  0.52 mm of the 1.1 mm allowed, one of them through a via crossover; the Ethernet pairs within
  0.01 mm of 0.25; the three MIPI pairs at 0.00 mm of 0.15. No crosstalk violations. KiCad DRC
  clean. Six gates. The engine turned every edge connector to face its edge from the footprint
  alone, moved the audio jack to the bottom edge when the top edge had no slot deep enough,
  and grew the 1.8 V plane island so its capacitors could reach it. The warnings it left are
  real and listed: crystal traces of 11 to 17 mm, one capacitor with a 7.4 nH loop, three
  capacitors beyond the island, a board density of 147 percent.

What "six gates" does not mean: none of these boards has been simulated or manufactured.
Connector pinouts follow common conventions and must be checked against the parts you buy.
3D bodies fetched from LCSC were not compared with physical parts. Skew between pairs (clock
to data) is not enforced. Everything is placed on the top side.

## Environment variables

| Variable | What it does |
|---|---|
| `SOMEDA_PLAN_FILE` | Build this plan instead of asking a model |
| `SOMEDA_PROJECT_DIR` | Where the outputs go (default `./someda-build`) |
| `SOMEDA_FOOTPRINT_DIRS` | Extra `.pretty` folders, searched before KiCad's |
| `SOMEDA_REPAIR_ROUNDS` | Repair rounds after the first build (default 3; 0 turns them off) |
| `SOMEDA_STOP_AFTER` | `schematic` or `route`: write that much and stop, for example to review a schematic first |
| `SOMEDA_KICAD_CLI` | Path to `kicad-cli` |
| `SOMEDA_API_KEY`, `SOMEDA_BASE_URL`, `SOMEDA_MODEL` | The planning model (the `OPENAI_*` names work too) |
| `SOMEDA_ROUTE_DEBUG` (`1` or `2`), `SOMEDA_ROUTE_DEBUG_NET`, `SOMEDA_PLACE_DEBUG` | Router and placer diagnostics: occupancy maps around failed pins, corridor decisions, placement targets |
| `KICAD10_3DMODEL_DIR` (or 9, 8) | Where the 3D bodies for the renders are, when KiCad is installed somewhere unusual |

## Working with existing KiCad files

The engine also reads boards and schematics you already have:

```
someda info       <board.kicad_pcb>          summary of the board
someda drc        <board.kicad_pcb>          design rule checks
someda validate   <board.kicad_pcb>          parse and check (exit 1 on errors)
someda repair     <board.kicad_pcb>          nudge pads inside the outline until the checks pass
someda report     <board.kicad_pcb> [json|md]
someda erc        <schematic.kicad_sch>      electrical rule check
someda netclasses <board.kicad_pcb>          net class summary
```

## Inside the repository

```
src/orchestrator.rs   the pipeline: plan, stages, gates
src/repair.rs         repair rounds and the plan edits they make
src/placer.rs         courtyard placement
src/router.rs         grid router; router/pairs.rs: differential-pair corridors
src/si.rs stability.rs pdn.rs thermal.rs   audits on the routed board
src/formulas.rs materials.rs netfreq.rs    physics, JLCPCB stackups, per-net frequency
src/footprint_lib.rs board.rs sexp.rs      KiCad footprints, board model, s-expressions
src/schematic.rs drc.rs rules.rs           schematic generation and ERC, engine DRC, fab rules
src/kicad_cli.rs bom.rs jlcpcb.rs          kicad-cli steps, BOM, JLCPCB package
src/app/              the desktop app's logic, with no GUI code, all unit-tested
src/bin/someda-app/   the window (egui), behind the "app" feature
examples/             plan_555.json; formula_probe prints every formula; app_e2e drives the app's flow without a window
tests/                a corpus of real boards and fixtures
```

`cargo test` runs about 220 tests. `cargo run --example formula_probe` prints the physics
outputs so you can compare them with independent references. Pushing a `v*` tag builds the
release binaries for all four platforms.

[ARCHITECTURE.md](ARCHITECTURE.md) explains the design and why "done" has to be earned.
[ROADMAP.md](ROADMAP.md) lists the milestones. [CHANGELOG.md](CHANGELOG.md) says what each
version changed and what it was verified against.

## Where it comes from

someda is an original implementation of the ideas documented in the
[SomTronics AI-EDA study](https://github.com/kabidey/somtronics-ai-eda-study) of a commercial
AI-EDA product. It uses the public KiCad file formats, and its rule checks and repair loop
were written from scratch. It contains no code, text or material from the product that was
studied. MIT license.
