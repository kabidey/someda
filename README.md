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
```

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
