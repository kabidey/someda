//! Netlist extraction: pin-level connectivity from the schematic, joined to
//! board pads by reference designator, producing the ratsnest for placement
//! scoring and routing.

use crate::board::Board;
use crate::schematic::{derive_nets, Schematic};

/// A ratsnest connection: two pads on the same net that need routing.
#[derive(Debug, Clone, PartialEq)]
pub struct RatsnestEdge {
    pub net: String,
    pub from_ref: String,
    pub from_pin: String,
    pub from_x: f64,
    pub from_y: f64,
    pub to_ref: String,
    pub to_pin: String,
    pub to_x: f64,
    pub to_y: f64,
    /// Manhattan distance in mm (placement difficulty signal)
    pub manhattan_mm: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Netlist {
    /// Nets keyed by name, each with its connected (ref, pin) pairs
    pub nets: Vec<(String, Vec<(String, String)>)>,
    /// All ratsnest edges (pad-to-pad connections to route)
    pub edges: Vec<RatsnestEdge>,
    /// Total ratsnest Manhattan length (placement quality metric)
    pub total_manhattan_mm: f64,
}

/// Extract the netlist from a schematic and join to board pads by reference.
pub fn extract(sch: &Schematic, board: &Board) -> Netlist {
    let schematic_nets = derive_nets(sch);
    let mut nets = Vec::new();
    let mut edges = Vec::new();
    let mut total = 0.0;

    for net in &schematic_nets {
        let name = net.name.clone().unwrap_or_else(|| format!("N{}", nets.len()));
        let mut pad_positions: Vec<((String, String), f64, f64)> = Vec::new();

        for (ref_, pin_num, _pin_name) in &net.pins {
            // Find this reference on the board
            if let Some(fp) = board.footprints.iter().find(|f| &f.reference == ref_) {
                if let Some(pad) = fp.pads.iter().find(|p| &p.number == pin_num) {
                    pad_positions.push((
                        (ref_.clone(), pin_num.clone()),
                        pad.x,
                        pad.y,
                    ));
                }
            }
        }

        // Generate ratsnest edges: chain pads in a minimum-Manhattan order (greedy nearest-neighbor)
        if pad_positions.len() >= 2 {
            let mut remaining: Vec<((String, String), f64, f64)> = pad_positions.clone();
            let mut current = remaining.remove(0);
            let mut chain = vec![current.clone()];
            while !remaining.is_empty() {
                // nearest by Manhattan
                let mut best_i = 0;
                let mut best_d = f64::INFINITY;
                for (i, r) in remaining.iter().enumerate() {
                    let d = (current.1 - r.1).abs() + (current.2 - r.2).abs();
                    if d < best_d {
                        best_d = d;
                        best_i = i;
                    }
                }
                let next = remaining.remove(best_i);
                edges.push(RatsnestEdge {
                    net: name.clone(),
                    from_ref: current.0 .0.clone(),
                    from_pin: current.0 .1.clone(),
                    from_x: current.1,
                    from_y: current.2,
                    to_ref: next.0 .0.clone(),
                    to_pin: next.0 .1.clone(),
                    to_x: next.1,
                    to_y: next.2,
                    manhattan_mm: best_d,
                });
                total += best_d;
                current = next;
            }
        }

        let connected: Vec<(String, String)> = net.pins.iter()
            .map(|(r, p, _)| (r.clone(), p.clone()))
            .collect();
        nets.push((name, connected));
    }

    Netlist { nets, edges, total_manhattan_mm: total }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sch() -> Schematic {
        Schematic::from_kicad_sch(r##"(kicad_sch
          (lib_symbols
            (symbol "Device:R"
              (symbol "R_0_1"
                (pin passive line (at -5.08 0 0) (length 3.81) (name "~") (number "1"))
                (pin passive line (at 5.08 0 180) (length 3.81) (name "~") (number "2"))
              )
            )
          )
          (symbol (lib_id "Device:R") (at 100 50 0)
            (property "Reference" "R1" (at 0 0 0))
          )
          (symbol (lib_id "Device:R") (at 120 50 0)
            (property "Reference" "R2" (at 0 0 0))
          )
          (wire (pts (xy 94.92 50) (xy 114.92 50)))
          (wire (pts (xy 105.08 50) (xy 125.08 50)))
        )"##).unwrap()
    }

    fn sample_board() -> Board {
        Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 200 0) (layer "Edge.Cuts"))
          (gr_line (start 200 0) (end 200 100) (layer "Edge.Cuts"))
          (gr_line (start 200 100) (end 0 100) (layer "Edge.Cuts"))
          (gr_line (start 0 100) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:R0805" (at 50 50 0)
            (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
            (pad "2" smd rect (at 1 0) (size 1.133 1.377) (net 2 "B"))
          )
          (footprint "somlib:R0805" (at 80 50 0)
            (fp_text reference "R2" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
            (pad "2" smd rect (at 1 0) (size 1.133 1.377) (net 2 "B"))
          )
        )"#).unwrap()
    }

    #[test]
    fn extracts_nets_and_ratsnest() {
        let sch = sample_sch();
        let board = sample_board();
        let nl = extract(&sch, &board);
        assert!(!nl.nets.is_empty());
        assert!(!nl.edges.is_empty(), "ratsnest should have edges");
        assert!(nl.total_manhattan_mm > 0.0);
        // Every edge references pads that exist
        for e in &nl.edges {
            assert!(board.footprints.iter().any(|f| f.reference == e.from_ref));
            assert!(board.footprints.iter().any(|f| f.reference == e.to_ref));
        }
    }

    #[test]
    fn ratsnest_manhattan_is_correct() {
        let sch = sample_sch();
        let board = sample_board();
        let nl = extract(&sch, &board);
        for e in &nl.edges {
            let expected = (e.from_x - e.to_x).abs() + (e.from_y - e.to_y).abs();
            assert!((e.manhattan_mm - expected).abs() < 1e-9);
        }
        assert!((nl.total_manhattan_mm - nl.edges.iter().map(|e| e.manhattan_mm).sum::<f64>()).abs() < 1e-9);
    }

    #[test]
    fn total_manhattan_decreases_with_closer_placement() {
        let sch = sample_sch();
        // Same board but R2 placed closer to R1
        let closer = Board::from_kicad_pcb(r#"(kicad_pcb
          (gr_line (start 0 0) (end 200 0) (layer "Edge.Cuts"))
          (gr_line (start 200 0) (end 200 100) (layer "Edge.Cuts"))
          (gr_line (start 200 100) (end 0 100) (layer "Edge.Cuts"))
          (gr_line (start 0 100) (end 0 0) (layer "Edge.Cuts"))
          (footprint "somlib:R0805" (at 50 50 0)
            (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
            (pad "2" smd rect (at 1 0) (size 1.133 1.377) (net 2 "B"))
          )
          (footprint "somlib:R0805" (at 52 50 0)
            (fp_text reference "R2" (at 0 0) (layer "F.SilkS"))
            (pad "1" smd rect (at -1 0) (size 1.133 1.377) (net 1 "A"))
            (pad "2" smd rect (at 1 0) (size 1.133 1.377) (net 2 "B"))
          )
        )"#).unwrap();
        let nl_far = extract(&sch, &sample_board());
        let nl_near = extract(&sch, &closer);
        assert!(nl_near.total_manhattan_mm < nl_far.total_manhattan_mm,
            "closer placement ({}) should have smaller ratsnest than far ({})",
            nl_near.total_manhattan_mm, nl_far.total_manhattan_mm);
    }
}
