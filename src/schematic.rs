//! Schematic model and ERC (Electrical Rule Check) engine.
//!
//! Parses KiCad `.kicad_sch` files: library symbols (with pin definitions),
//! placed instances, wires, labels, global labels, power symbols, and
//! no-connect markers. Re-derives connectivity from geometry (pin absolute
//! positions + wire endpoints), then runs electrical checks:
//! floating pins, unconnected nets, missing power flags.

use crate::sexp::{parse, Sexp};

/// A pin from a library symbol definition: relative to the symbol origin.
#[derive(Debug, Clone)]
pub struct LibPin {
    pub name: String,
    pub number: String,
    pub electric_type: String, // passive, input, output, power_in, ...
    pub x: f64,
    pub y: f64,
    pub rot_deg: f64,
}

/// A library symbol definition.
#[derive(Debug, Clone)]
pub struct LibSymbol {
    pub name: String, // "lib:NAME"
    pub pins: Vec<LibPin>,
}

/// A placed symbol instance in the schematic.
#[derive(Debug, Clone)]
pub struct Instance {
    pub lib_id: String,
    pub reference: String,
    pub value: String,
    pub x: f64,
    pub y: f64,
    pub rot_deg: f64,
    pub is_power: bool, // lib_id starts with "power:"
}

/// A wire segment.
#[derive(Debug, Clone)]
pub struct Wire {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

/// A label (local or global) attached to a point.
#[derive(Debug, Clone)]
pub struct Label {
    pub name: String,
    pub global: bool,
    pub x: f64,
    pub y: f64,
}

/// A no-connect marker.
#[derive(Debug, Clone)]
pub struct NoConnect {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Schematic {
    pub lib_symbols: Vec<LibSymbol>,
    pub instances: Vec<Instance>,
    pub wires: Vec<Wire>,
    pub labels: Vec<Label>,
    pub no_connects: Vec<NoConnect>,
}

impl Schematic {
    pub fn from_kicad_sch(input: &str) -> Result<Schematic, String> {
        let doc = parse(input).map_err(|e| e.to_string())?;
        let root = doc
            .as_tagged("kicad_sch")
            .ok_or("root element is not (kicad_sch ...)")?;
        let mut sch = Schematic::default();

        for item in root {
            // Library symbols: (lib_symbols (symbol "lib:NAME" ... pins ...))
            if let Some(libs) = item.as_tagged("lib_symbols") {
                for lib in libs {
                    if let Some(sym) = parse_lib_symbol(lib) {
                        sch.lib_symbols.push(sym);
                    }
                }
            }
            // Placed instances: (symbol (lib_id "...") (at ...) ...)
            if item.as_tagged("symbol").is_some() {
                if let Some(inst) = parse_instance(item) {
                    sch.instances.push(inst);
                }
            }
            // Wires: (wire (pts (xy x1 y1) (xy x2 y2)))
            if let Some(w) = item.as_tagged("wire") {
                if let Some(wire) = parse_wire(w) {
                    sch.wires.push(wire);
                }
            }
            // Labels: (label "name" (at x y ...)) and (global_label ...)
            if let Some(l) = item.as_tagged("label") {
                if let Some(lbl) = parse_label(l, false) {
                    sch.labels.push(lbl);
                }
            }
            if let Some(l) = item.as_tagged("global_label") {
                if let Some(lbl) = parse_label(l, true) {
                    sch.labels.push(lbl);
                }
            }
            // No-connect: (no_connect (at x y) ...)
            if let Some(nc) = item.as_tagged("no_connect") {
                if nc.len() >= 1 {
                    if let Some((x, y, _)) = nc[0].as_tagged("at").and_then(|a| {
                        a.first().zip(a.get(1)).and_then(|(x, y)| Some((x.f64_value()?, y.f64_value()?)))
                    }).map(|(x, y)| (x, y, 0.0)) {
                        sch.no_connects.push(NoConnect { x, y });
                    }
                }
            }
        }
        Ok(sch)
    }

    /// Absolute position of a pin on a placed instance.
    pub fn pin_absolute(&self, inst: &Instance, pin: &LibPin) -> (f64, f64) {
        let r = inst.rot_deg.to_radians();
        let ox = pin.x * r.cos() - pin.y * r.sin();
        let oy = pin.x * r.sin() + pin.y * r.cos();
        (inst.x + ox, inst.y + oy)
    }

    /// All pins of an instance, resolved from its lib symbol.
    pub fn instance_pins(&self, inst: &Instance) -> Vec<(String, String, f64, f64)> {
        // lib_id may be "lib:NAME" or "lib:NAME_N" (unit variant); match base name
        let base = inst.lib_id.rsplit(':').next().unwrap_or("");
        let base = base.split('_').next().unwrap_or(base);
        for lib in &self.lib_symbols {
            let lib_base = lib.name.rsplit(':').next().unwrap_or("");
            let lib_base = lib_base.split('_').next().unwrap_or(lib_base);
            if lib_base == base {
                return lib.pins.iter().map(|p| {
                    let (ax, ay) = self.pin_absolute(inst, p);
                    (p.number.clone(), p.name.clone(), ax, ay)
                }).collect();
            }
        }
        Vec::new()
    }
}

fn parse_lib_symbol(item: &Sexp) -> Option<LibSymbol> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let name = items.get(1).and_then(|s| s.str_value())?.trim_matches('"').to_string();
    let mut pins = Vec::new();
    collect_pins(item, &mut pins);
    Some(LibSymbol { name, pins })
}

fn collect_pins(node: &Sexp, pins: &mut Vec<LibPin>) {
    let items = match node { Sexp::List(v) => v, _ => return };
    if let Some(p) = node.as_tagged("pin") {
        // (pin TYPE STYLE (at x y rot) (length l) (name "n" ...) (number "n" ...))
        let electric_type = p.get(0).and_then(|s| s.str_value()).unwrap_or("").to_string();
        let mut at = (0.0, 0.0, 0.0);
        let mut name = String::new();
        let mut number = String::new();
        for sub in p {
            if let Some(a) = sub.as_tagged("at") {
                if a.len() >= 2 {
                    if let (Some(x), Some(y)) = (a[0].f64_value(), a[1].f64_value()) {
                        at = (x, y, a.get(2).and_then(|s| s.f64_value()).unwrap_or(0.0));
                    }
                }
            }
            if let Some(n) = sub.as_tagged("name") {
                name = n.first().and_then(|s| s.str_value()).unwrap_or("").trim_matches('"').to_string();
            }
            if let Some(n) = sub.as_tagged("number") {
                number = n.first().and_then(|s| s.str_value()).unwrap_or("").trim_matches('"').to_string();
            }
        }
        if !number.is_empty() || !name.is_empty() {
            pins.push(LibPin { name, number, electric_type, x: at.0, y: at.1, rot_deg: at.2 });
        }
        return;
    }
    for child in items {
        collect_pins(child, pins);
    }
}

fn parse_instance(item: &Sexp) -> Option<Instance> {
    let items = match item { Sexp::List(v) => v, _ => return None };
    let lib_id = items.iter()
        .find_map(|c| c.as_tagged("lib_id"))
        .and_then(|l| l.first())
        .and_then(|s| s.str_value())?
        .trim_matches('"')
        .to_string();

    let mut at = (0.0, 0.0, 0.0);
    let mut reference = String::new();
    let mut value = String::new();

    for child in items {
        if let Some(a) = child.as_tagged("at") {
            if a.len() >= 2 {
                if let (Some(x), Some(y)) = (a[0].f64_value(), a[1].f64_value()) {
                    at = (x, y, a.get(2).and_then(|s| s.f64_value()).unwrap_or(0.0));
                }
            }
        }
        if let Some(props) = child.as_tagged("property") {
            let key = props.first().and_then(|s| s.str_value()).unwrap_or("");
            let val = props.get(1).and_then(|s| s.str_value()).unwrap_or("");
            match key {
                "Reference" => reference = val.trim_matches('"').to_string(),
                "Value" => value = val.trim_matches('"').to_string(),
                _ => {}
            }
        }
    }

    let is_power = lib_id.starts_with("power:") || reference.starts_with('#');
    Some(Instance { lib_id, reference, value, x: at.0, y: at.1, rot_deg: at.2, is_power })
}

fn parse_wire(args: &[Sexp]) -> Option<Wire> {
    // (wire (pts (xy x1 y1) (xy x2 y2)) ...)
    for arg in args {
        if let Some(pts) = arg.as_tagged("pts") {
            let p1 = pts.first()?;
            let p2 = pts.get(1)?;
            let xy1 = p1.as_tagged("xy")?;
            let xy2 = p2.as_tagged("xy")?;
            return Some(Wire {
                x1: xy1.first()?.f64_value()?,
                y1: xy1.get(1)?.f64_value()?,
                x2: xy2.first()?.f64_value()?,
                y2: xy2.get(1)?.f64_value()?,
            });
        }
    }
    None
}

fn parse_label(args: &[Sexp], global: bool) -> Option<Label> {
    let name = args.first()?.str_value()?.trim_matches('"').to_string();
    let mut x = 0.0;
    let mut y = 0.0;
    for arg in args {
        if let Some(a) = arg.as_tagged("at") {
            x = a.first().and_then(|s| s.f64_value()).unwrap_or(0.0);
            y = a.get(1).and_then(|s| s.f64_value()).unwrap_or(0.0);
        }
    }
    Some(Label { name, global, x, y })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"(kicad_sch (version 20230121)
      (lib_symbols
        (symbol "Device:R" (pin_names (offset 0)) (in_bom yes) (on_board yes)
          (symbol "R_0_1"
            (pin passive line (at -5.08 0 0) (length 3.81)
              (name "~" (effects (font (size 1.27 1.27))))
              (number "1" (effects (font (size 1.27 1.27))))
            )
            (pin passive line (at 5.08 0 180) (length 3.81)
              (name "~" (effects (font (size 1.27 1.27))))
              (number "2" (effects (font (size 1.27 1.27))))
            )
          )
        )
      )
      (symbol (lib_id "Device:R") (at 100 50 0) (unit 1)
        (property "Reference" "R1" (at 0 0 0))
        (property "Value" "10k" (at 0 0 0))
      )
      (wire (pts (xy 94.92 50) (xy 90 50)))
      (label "SIG_A" (at 90 50 0))
      (no_connect (at 105.08 50))
    )"#;

    #[test]
    fn parses_schematic() {
        let s = Schematic::from_kicad_sch(SAMPLE).unwrap();
        assert_eq!(s.lib_symbols.len(), 1);
        assert_eq!(s.lib_symbols[0].pins.len(), 2);
        assert_eq!(s.instances.len(), 1);
        assert_eq!(s.instances[0].reference, "R1");
        assert_eq!(s.wires.len(), 1);
        assert_eq!(s.labels.len(), 1);
        assert_eq!(s.labels[0].name, "SIG_A");
        assert_eq!(s.no_connects.len(), 1);
    }

    #[test]
    fn instance_pins_absolute() {
        let s = Schematic::from_kicad_sch(SAMPLE).unwrap();
        let pins = s.instance_pins(&s.instances[0]);
        assert_eq!(pins.len(), 2);
        // pin 1 at rel (-5.08, 0), instance at (100, 50, 0) → abs (94.92, 50)
        assert!((pins[0].2 - 94.92).abs() < 1e-9);
        assert!((pins[0].3 - 50.0).abs() < 1e-9);
        // pin 2 at rel (5.08, 0) → abs (105.08, 50)
        assert!((pins[1].2 - 105.08).abs() < 1e-9);
    }

    #[test]
    fn rotated_instance_pins() {
        let input = r#"(kicad_sch
          (lib_symbols
            (symbol "Device:R"
              (symbol "R_0_1"
                (pin passive line (at 5.08 0 180) (length 3.81) (name "~") (number "2"))
              )
            )
          )
          (symbol (lib_id "Device:R") (at 100 50 90)
            (property "Reference" "R1" (at 0 0 0))
          )
        )"#;
        let s = Schematic::from_kicad_sch(input).unwrap();
        let pins = s.instance_pins(&s.instances[0]);
        // pin at rel (5.08, 0) rotated 90° → (0, 5.08) → abs (100, 55.08)
        assert!((pins[0].2 - 100.0).abs() < 1e-9);
        assert!((pins[0].3 - 55.08).abs() < 1e-9);
    }

    #[test]
    fn power_symbol_detected() {
        let input = r##"(kicad_sch
          (symbol (lib_id "power:GND") (at 50 50 0)
            (property "Reference" "#PWR01" (at 0 0 0))
          )
        )"##;
        let s = Schematic::from_kicad_sch(input).unwrap();
        assert!(s.instances[0].is_power);
    }
}

// ---------------------------------------------------------------------------
// ERC: Electrical Rule Check
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};

/// A net derived from connectivity: a set of connected pin positions with labels.
#[derive(Debug, Clone, Default)]
pub struct Net {
    pub name: Option<String>,
    /// (instance_reference, pin_number, pin_name)
    pub pins: Vec<(String, String, String)>,
    pub has_power: bool,
    pub is_no_connect: bool,
}

#[derive(Debug, Clone)]
pub struct ErcFinding {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct ErcReport {
    pub findings: Vec<ErcFinding>,
}

impl ErcReport {
    pub fn errors(&self) -> usize {
        self.findings.len()
    }
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Union-find for connecting points.
struct DisjointSets {
    parent: Vec<usize>,
}

impl DisjointSets {
    fn new(n: usize) -> Self {
        DisjointSets { parent: (0..n).collect() }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

fn points_match(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
}

/// Re-derive connectivity from geometry: pins, wires, labels, power symbols, no-connects.
pub fn derive_nets(sch: &Schematic) -> Vec<Net> {
    // Collect all points: pin positions, wire endpoints, label positions, no-connect positions
    #[derive(Clone)]
    struct Point {
        pos: (f64, f64),
        pin: Option<(String, String, String)>, // (ref, number, name)
        label: Option<String>,
        power: bool,
        no_connect: bool,
    }
    let mut points: Vec<Point> = Vec::new();

    for inst in &sch.instances {
        for (num, name, ax, ay) in sch.instance_pins(inst) {
            points.push(Point {
                pos: (ax, ay),
                pin: Some((inst.reference.clone(), num, name)),
                label: None,
                power: inst.is_power,
                no_connect: false,
            });
        }
    }
    let wire_point_indices: Vec<(usize, usize)> = sch.wires.iter().map(|w| {
        let i1 = points.len();
        points.push(Point { pos: (w.x1, w.y1), pin: None, label: None, power: false, no_connect: false });
        let i2 = points.len();
        points.push(Point { pos: (w.x2, w.y2), pin: None, label: None, power: false, no_connect: false });
        (i1, i2)
    }).collect();
    for l in &sch.labels {
        points.push(Point { pos: (l.x, l.y), pin: None, label: Some(l.name.clone()), power: false, no_connect: false });
    }
    for nc in &sch.no_connects {
        points.push(Point { pos: (nc.x, nc.y), pin: None, label: None, power: false, no_connect: true });
    }

    // Union points that coincide geometrically (pins/labels/nc at same position)
    // and wire endpoints that lie on other points.
    let n = points.len();
    let mut ds = DisjointSets::new(n);
    // A wire electrically connects its two endpoints
    for (i1, i2) in &wire_point_indices {
        ds.union(*i1, *i2);
    }
    // Coincident points (pin on wire end, label on wire end, pin on pin) also connect
    for i in 0..n {
        for j in (i + 1)..n {
            if points_match(points[i].pos, points[j].pos) {
                ds.union(i, j);
            }
        }
    }
    // Wire endpoint to wire endpoint coincidence (also handles T-junctions at same coords)
    // Note: midpoint-on-wire connections require full geometry; v0 uses endpoint coincidence
    // (matches the common case: pins connect at wire endpoints).

    // Collect nets by root
    let mut net_map: HashMap<usize, Net> = HashMap::new();
    for (idx, p) in points.iter().enumerate() {
        let root = ds.find(idx);
        let net = net_map.entry(root).or_default();
        if let Some((ref_, num, name)) = &p.pin {
            net.pins.push((ref_.clone(), num.clone(), name.clone()));
            if p.power {
                net.has_power = true;
            }
        }
        if let Some(l) = &p.label {
            if net.name.is_none() {
                net.name = Some(l.clone());
            }
        }
        if p.no_connect {
            net.is_no_connect = true;
        }
    }
    // Also: wires whose both endpoints connect nothing (islands) still form a "net" — skip empty ones
    net_map.into_values()
        .filter(|n| {
            // A net exists if it has: a label (named net), power, no-connect intent,
            // or more than one electrical feature. A completely lone pin (no wire,
            // no label, no power, no other pin at the same point) is floating — not a net.
            n.name.is_some() || n.has_power || n.is_no_connect
                || n.pins.len() > 1
                || (n.pins.len() == 1 && n.name.is_some())
        })
        .collect()
}

/// Run ERC v0: floating pins, unconnected nets, power-audit.
pub fn run_erc(sch: &Schematic) -> ErcReport {
    let mut findings = Vec::new();
    let nets = derive_nets(sch);

    // 1. Floating pins: any pin not in any derived net, and not covered by a no-connect
    let mut connected_pins: HashSet<(String, String)> = HashSet::new();
    let mut nc_positions: Vec<(f64, f64)> = sch.no_connects.iter().map(|nc| (nc.x, nc.y)).collect();
    for net in &nets {
        for (ref_, num, _) in &net.pins {
            connected_pins.insert((ref_.clone(), num.clone()));
        }
    }
    for inst in &sch.instances {
        for (num, _name, ax, ay) in sch.instance_pins(inst) {
            if inst.is_power {
                continue; // power symbols don't need connections themselves
            }
            if connected_pins.contains(&(inst.reference.clone(), num.clone())) {
                continue;
            }
            // check no-connect marker at this pin position
            let has_nc = nc_positions.iter().any(|&(nx, ny)| points_match((ax, ay), (nx, ny)));
            if !has_nc {
                findings.push(ErcFinding {
                    code: "floating-pin".into(),
                    message: format!("Pin {}:{} ('{}') is not connected to any net and has no no-connect marker.",
                        inst.reference, num, _name),
                });
            }
        }
    }

    // 2. Unconnected nets: nets with only ONE pin (single-pin nets are dead ends)
    for net in &nets {
        if net.pins.len() == 1 && !net.is_no_connect && !net.has_power {
            let (ref_, num, _) = &net.pins[0];
            findings.push(ErcFinding {
                code: "unconnected-net".into(),
                message: format!("Net '{}' (label: {:?}) has only one connection: {}:{} — signal goes nowhere.",
                    net.name.as_deref().unwrap_or("<unnamed>"), net.name, ref_, num),
            });
        }
    }

    // 3. Missing power flag: power symbols present but a net named like power has no driver.
    //    v0: check that any net containing a power_in/power_out pin has a power symbol connection.
    for net in &nets {
        let name = net.name.as_deref().unwrap_or("");
        let looks_like_power = name.eq_ignore_ascii_case("vcc") || name.eq_ignore_ascii_case("vdd")
            || name.eq_ignore_ascii_case("gnd") || name.eq_ignore_ascii_case("vss")
            || name.starts_with('+') || name.starts_with('-');
        if looks_like_power && !net.has_power && net.pins.len() > 1 {
            findings.push(ErcFinding {
                code: "missing-power-flag".into(),
                message: format!("Net '{}' looks like a power net but has no power symbol attached.",
                    name),
            });
        }
    }

    ErcReport { findings }
}

impl std::fmt::Display for ErcReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "ERC: {} finding(s)", self.errors())?;
        for finding in &self.findings {
            writeln!(f, "  [{}]: {}", finding.code, finding.message)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod erc_tests {
    use super::*;

    #[test]
    fn floating_pin_detected() {
        // R1 with pin 1 wired but pin 2 floating
        let sch = Schematic::from_kicad_sch(r##"(kicad_sch
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
          (wire (pts (xy 94.92 50) (xy 90 50)))
          (label "SIG" (at 90 50 0))
        )"##).unwrap();
        let report = run_erc(&sch);
        let floating: Vec<_> = report.findings.iter().filter(|f| f.code == "floating-pin").collect();
        assert_eq!(floating.len(), 1, "{:?}", report.findings);
        assert!(floating[0].message.contains("R1:2"));
    }

    #[test]
    fn no_connect_suppresses_floating() {
        let sch = Schematic::from_kicad_sch(r##"(kicad_sch
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
          (wire (pts (xy 94.92 50) (xy 90 50)))
          (label "SIG" (at 90 50 0))
          (no_connect (at 105.08 50))
        )"##).unwrap();
        let report = run_erc(&sch);
        assert!(report.findings.iter().all(|f| f.code != "floating-pin"), "{:?}", report.findings);
    }

    #[test]
    fn connected_pin_not_flagged() {
        // Both pins wired → no floating
        let sch = Schematic::from_kicad_sch(r##"(kicad_sch
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
          (wire (pts (xy 94.92 50) (xy 90 50)))
          (wire (pts (xy 105.08 50) (xy 110 50)))
          (label "A" (at 90 50 0))
          (label "B" (at 110 50 0))
        )"##).unwrap();
        let report = run_erc(&sch);
        let floating: Vec<_> = report.findings.iter().filter(|f| f.code == "floating-pin").collect();
        assert!(floating.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn single_pin_net_flagged() {
        // A net with only one connection (label + one pin, no second pin)
        let sch = Schematic::from_kicad_sch(r##"(kicad_sch
          (lib_symbols
            (symbol "Device:R"
              (symbol "R_0_1"
                (pin passive line (at -5.08 0 0) (length 3.81) (name "~") (number "1"))
              )
            )
          )
          (symbol (lib_id "Device:R") (at 100 50 0)
            (property "Reference" "R1" (at 0 0 0))
          )
          (wire (pts (xy 94.92 50) (xy 90 50)))
          (label "LONE" (at 90 50 0))
        )"##).unwrap();
        let report = run_erc(&sch);
        let unconn: Vec<_> = report.findings.iter().filter(|f| f.code == "unconnected-net").collect();
        assert!(!unconn.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn power_symbol_satisfies_power_check() {
        let sch = Schematic::from_kicad_sch(r##"(kicad_sch
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
          (wire (pts (xy 94.92 50) (xy 90 50)))
          (label "VCC" (at 90 50 0))
          (wire (pts (xy 105.08 50) (xy 110 50)))
          (label "GND" (at 110 50 0))
          (symbol (lib_id "power:VCC") (at 90 50 0)
            (property "Reference" "#PWR01" (at 0 0 0))
          )
          (symbol (lib_id "power:GND") (at 110 50 0)
            (property "Reference" "#PWR02" (at 0 0 0))
          )
        )"##).unwrap();
        let report = run_erc(&sch);
        let missing_power: Vec<_> = report.findings.iter().filter(|f| f.code == "missing-power-flag").collect();
        assert!(missing_power.is_empty(), "{:?}", report.findings);
    }
}
