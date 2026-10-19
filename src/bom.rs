//! Detailed bill of materials from the plan: one line per distinct
//! (value, footprint, part number), with quantity, reference designators,
//! manufacturer part number, LCSC part number, footprint and the package
//! read from the footprint name — written as CSV and Markdown next to the
//! Gerbers so the order and the board can be checked against each other.

use crate::orchestrator::CircuitPlan;

#[derive(Debug, Clone, PartialEq)]
pub struct BomLine {
    pub item: usize,
    pub qty: usize,
    pub references: Vec<String>,
    pub value: String,
    pub mpn: String,
    pub lcsc: String,
    pub lib_id: String,
    pub footprint: String,
    pub package: String,
}

/// The package a KiCad footprint name describes: "C_0402_1005Metric" →
/// "0402 (1005 metric)", "SOT-23-6" → "SOT-23-6", "LQFP-128_14x14mm_P0.4mm"
/// → "LQFP-128 14x14mm P0.4mm", connectors keep their family name.
pub fn package_from_footprint(footprint: &str) -> String {
    let name = footprint.split(':').last().unwrap_or(footprint);
    // Chip passives: X_NNNN_MMMMMetric
    let parts: Vec<&str> = name.split('_').collect();
    if name.starts_with("LED_") && parts.len() >= 3 && parts[2].ends_with("Metric") {
        return format!("LED {} ({} metric)", parts[1], parts[2].trim_end_matches("Metric"));
    }
    if parts.len() >= 3 && parts[1].chars().all(|c| c.is_ascii_digit()) && parts[2].ends_with("Metric") {
        return format!("{} ({} metric)", parts[1], parts[2].trim_end_matches("Metric"));
    }
    // Packages with size/pitch suffixes: keep the readable form
    name.replace('_', " ")
}

pub fn build(plan: &CircuitPlan) -> Vec<BomLine> {
    let mut lines: Vec<BomLine> = Vec::new();
    for c in &plan.components {
        if c.reference.starts_with('H') && plan.nets.iter().all(|n| !n.pins.iter().any(|p| p.starts_with(&format!("{}:", c.reference)))) {
            // mounting holes: mechanical, not ordered
            continue;
        }
        let fp = c.footprint.clone().unwrap_or_default();
        let mpn = c.mpn.clone().unwrap_or_default();
        let lcsc = c.lcsc.clone().unwrap_or_default();
        match lines.iter_mut().find(|l| l.value == c.value && l.footprint == fp && l.mpn == mpn && l.lcsc == lcsc) {
            Some(l) => {
                l.qty += 1;
                l.references.push(c.reference.clone());
            }
            None => lines.push(BomLine {
                item: 0,
                qty: 1,
                references: vec![c.reference.clone()],
                value: c.value.clone(),
                mpn,
                lcsc,
                lib_id: c.lib_id.clone(),
                footprint: fp.clone(),
                package: package_from_footprint(&fp),
            }),
        }
    }
    // Order: ICs, then connectors, then passives by value — stable for diffs
    let class = |r: &str| match r.chars().next() { Some('U') => 0, Some('J') => 1, Some('Y') => 2, Some('L') => 3, Some('D') => 4, Some('Q') => 5, Some('S') => 6, Some('F') => 7, Some('C') => 8, Some('R') => 9, _ => 10 };
    lines.sort_by(|a, b| class(&a.references[0]).cmp(&class(&b.references[0])).then_with(|| a.value.cmp(&b.value)).then_with(|| a.footprint.cmp(&b.footprint)));
    for (i, l) in lines.iter_mut().enumerate() {
        l.item = i + 1;
        l.references.sort_by(|a, b| {
            let num = |s: &str| s.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u32>().unwrap_or(0);
            a.trim_end_matches(|c: char| c.is_ascii_digit()).cmp(b.trim_end_matches(|c: char| c.is_ascii_digit())).then(num(a).cmp(&num(b)))
        });
    }
    lines
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn to_csv(lines: &[BomLine]) -> String {
    let mut out = String::from("Item,Qty,References,Value,MPN,LCSC,Package,Footprint,Symbol\n");
    for l in lines {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            l.item, l.qty, csv_escape(&l.references.join(" ")), csv_escape(&l.value), csv_escape(&l.mpn), csv_escape(&l.lcsc), csv_escape(&l.package), csv_escape(&l.footprint), csv_escape(&l.lib_id)
        ));
    }
    out
}

pub fn to_markdown(plan_name: &str, lines: &[BomLine]) -> String {
    let total: usize = lines.iter().map(|l| l.qty).sum();
    let mut out = format!("# Bill of materials — {}\n\n{} line items, {} components. MPN and LCSC columns come from the plan (`mpn`, `lcsc`); empty means a generic part ordered by value and package.\n\n", plan_name, lines.len(), total);
    out.push_str("| # | Qty | References | Value | MPN | LCSC | Package | Footprint |\n|---|---|---|---|---|---|---|---|\n");
    for l in lines {
        out.push_str(&format!("| {} | {} | {} | {} | {} | {} | {} | `{}` |\n", l.item, l.qty, l.references.join(", "), l.value, l.mpn, l.lcsc, l.package, l.footprint));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_read_from_footprint_names() {
        assert_eq!(package_from_footprint("Capacitor_SMD:C_0402_1005Metric"), "0402 (1005 metric)");
        assert_eq!(package_from_footprint("Package_TO_SOT_SMD:SOT-23-6"), "SOT-23-6");
        assert_eq!(package_from_footprint("Package_QFP:LQFP-128_14x14mm_P0.4mm"), "LQFP-128 14x14mm P0.4mm");
        assert_eq!(package_from_footprint("LED_SMD:LED_0603_1608Metric"), "LED 0603 (1608 metric)");
    }

    #[test]
    fn groups_identical_parts_and_counts_them() {
        let plan: CircuitPlan = serde_json::from_str(r#"{"name":"t","description":"","components":[
            {"reference":"C1","value":"100nF","lib_id":"Device:C","footprint":"Capacitor_SMD:C_0402_1005Metric"},
            {"reference":"C2","value":"100nF","lib_id":"Device:C","footprint":"Capacitor_SMD:C_0402_1005Metric"},
            {"reference":"C10","value":"100nF","lib_id":"Device:C","footprint":"Capacitor_SMD:C_0402_1005Metric"},
            {"reference":"U1","value":"TPS562200","lib_id":"Regulator_Switching:TPS562200","footprint":"Package_TO_SOT_SMD:SOT-23-6","mpn":"TPS562200DDCR"},
            {"reference":"H1","value":"M3","lib_id":"Mechanical:MountingHole","footprint":"MountingHole:MountingHole_3.2mm_M3"}
        ],"nets":[{"name":"GND","pins":["C1:2","C2:2","C10:2","U1:1"]}]}"#).unwrap();
        let lines = build(&plan);
        assert_eq!(lines.len(), 2, "{:?}", lines);
        assert_eq!(lines[0].references, vec!["U1"]);
        assert_eq!(lines[1].qty, 3);
        assert_eq!(lines[1].references, vec!["C1", "C2", "C10"], "numeric reference order");
        assert_eq!(lines[0].mpn, "TPS562200DDCR");
        let csv = to_csv(&lines);
        assert!(csv.contains("2,3,C1 C2 C10,100nF,,,0402 (1005 metric),Capacitor_SMD:C_0402_1005Metric,Device:C"));
    }
}
