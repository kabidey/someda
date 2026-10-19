//! JLCPCB fabrication package: what the order page accepts, written from
//! the verified board.
//!
//! - Gerbers with Protel extensions, no X2 attributes, no netlist attributes,
//!   silkscreen subtracted by the solder mask, zones checked (JLCPCB's KiCad
//!   guide); Excellon drill with PTH and NPTH merged, decimal format, mm,
//!   plus the PDF map — all zipped as `jlcpcb-gerbers.zip`.
//! - `jlcpcb-bom.csv`: Comment, Designator, Footprint, LCSC Part # (their
//!   assembly BOM columns), from the plan's values and `lcsc` fields.
//! - `jlcpcb-cpl.csv`: Designator, Mid X, Mid Y, Layer, Rotation — the
//!   pick-and-place file with per-package rotation corrections applied, so
//!   JLCPCB's part orientation (EasyEDA convention) matches KiCad's
//!   footprint orientation. The corrections are the community table used
//!   by the JLCKicadTools / kicad-jlcpcb-tools projects; every applied
//!   correction is listed so the assembly preview can be checked.

use std::io::Write;

/// (footprint-name prefix match, degrees to ADD to KiCad's rotation)
/// Sources: JLCKicadTools `cpl_rotations_db.csv` (matthewlai, 2019–2023)
/// as commonly reproduced; verify on the JLCPCB SMT preview before ordering.
pub const ROTATION_CORRECTIONS: &[(&str, f64)] = &[
    ("SOT-23-5", 180.0),
    ("SOT-23-6", 180.0),
    ("SOT-23-8", 180.0),
    ("SOT-23", 180.0),
    ("SOT-353", 180.0),
    ("SOT-363", 180.0),
    ("SOT-89", 180.0),
    ("SOT-223", 180.0),
    ("SOIC-8", 270.0),
    ("SOIC-14", 270.0),
    ("SOIC-16", 270.0),
    ("SOP-", 270.0),
    ("SSOP-", 270.0),
    ("TSSOP-", 270.0),
    ("MSOP-", 270.0),
    ("HTSSOP-", 270.0),
    ("VSSOP-", 270.0),
    ("LQFP-", 270.0),
    ("TQFP-", 270.0),
    ("QFN-", 270.0),
    ("DFN-", 270.0),
    ("eLQFP-", 270.0),
    ("D_SOD-123", 180.0),
    ("D_SOD-323", 180.0),
    ("D_SMA", 180.0),
    ("D_SMB", 180.0),
    ("CP_EIA-", 180.0),
    ("CP_Elec_", 180.0),
    ("LED_0603", 180.0),
    ("LED_0805", 180.0),
    ("Crystal_SMD_3225", 180.0),
];

/// Rotation correction (degrees) for a footprint, by its library name.
pub fn rotation_correction(footprint: &str) -> f64 {
    let name = footprint.split(':').last().unwrap_or(footprint);
    for (prefix, deg) in ROTATION_CORRECTIONS {
        if name.starts_with(prefix) {
            return *deg;
        }
    }
    0.0
}

/// One CPL row in JLCPCB's format.
#[derive(Debug, Clone, PartialEq)]
pub struct CplRow {
    pub designator: String,
    pub mid_x: f64,
    pub mid_y: f64,
    pub layer: String,
    pub rotation: f64,
    pub correction: f64,
}

/// kicad-cli's CSV position file → CPL rows with corrections applied.
/// The position CSV columns are Ref,Val,Package,PosX,PosY,Rot,Side.
pub fn cpl_from_positions(pos_csv: &str, footprint_of: &dyn Fn(&str) -> Option<String>) -> Vec<CplRow> {
    let mut rows = Vec::new();
    for (i, line) in pos_csv.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<String> = split_csv(line);
        if cols.len() < 7 {
            continue;
        }
        let refdes = cols[0].clone();
        let (x, y, rot) = (cols[3].parse::<f64>().unwrap_or(0.0), cols[4].parse::<f64>().unwrap_or(0.0), cols[5].parse::<f64>().unwrap_or(0.0));
        let side = if cols[6].to_ascii_lowercase().starts_with('b') { "Bottom" } else { "Top" };
        let correction = footprint_of(&refdes).map(|f| rotation_correction(&f)).unwrap_or(0.0);
        let mut r = (rot + correction) % 360.0;
        if r < 0.0 {
            r += 360.0;
        }
        rows.push(CplRow { designator: refdes, mid_x: x, mid_y: y, layer: side.into(), rotation: r, correction });
    }
    rows
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    for ch in line.chars() {
        match ch {
            '"' => q = !q,
            ',' if !q => {
                out.push(cur.clone());
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

pub fn cpl_csv(rows: &[CplRow]) -> String {
    let mut s = String::from("Designator,Mid X,Mid Y,Layer,Rotation\n");
    for r in rows {
        s.push_str(&format!("{},{:.4}mm,{:.4}mm,{},{:.0}\n", r.designator, r.mid_x, r.mid_y, r.layer, r.rotation));
    }
    s
}

/// JLCPCB assembly BOM: Comment, Designator, Footprint, LCSC Part #.
pub fn bom_csv(lines: &[crate::bom::BomLine]) -> String {
    let mut s = String::from("Comment,Designator,Footprint,LCSC Part #\n");
    for l in lines {
        let q = |t: &str| if t.contains(',') { format!("\"{}\"", t) } else { t.to_string() };
        s.push_str(&format!("{},{},{},{}\n", q(&l.value), q(&l.references.join(",")), q(l.footprint.split(':').last().unwrap_or(&l.footprint)), l.lcsc));
    }
    s
}

/// A stored (uncompressed) zip of the given files — no external crate, no
/// external tool; JLCPCB's uploader reads it like any zip.
pub fn write_store_zip(zip_path: &std::path::Path, files: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::new();
    for (name, data) in files {
        let crc = crc32(data);
        offsets.push(out.len() as u32);
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0x00]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    for (i, (name, data)) in files.iter().enumerate() {
        let crc = crc32(data);
        central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0x00]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&offsets[i].to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    let mut f = std::fs::File::create(zip_path)?;
    f.write_all(&out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        table[i as usize] = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections_follow_the_package_family() {
        assert_eq!(rotation_correction("Package_TO_SOT_SMD:SOT-23-6"), 180.0);
        assert_eq!(rotation_correction("Package_QFP:LQFP-128_14x14mm_P0.4mm"), 270.0);
        assert_eq!(rotation_correction("Capacitor_SMD:C_0402_1005Metric"), 0.0);
    }

    #[test]
    fn cpl_applies_corrections_and_normalises() {
        let pos = "Ref,Val,Package,PosX,PosY,Rot,Side\n\"U2\",\"TPS562200\",\"SOT-23-6\",10.000000,20.000000,270.000000,top\n\"C1\",\"100nF\",\"C_0402_1005Metric\",1.0,2.0,-90.0,bottom\n";
        let fp = |r: &str| if r == "U2" { Some("Package_TO_SOT_SMD:SOT-23-6".to_string()) } else { Some("Capacitor_SMD:C_0402_1005Metric".to_string()) };
        let rows = cpl_from_positions(pos, &fp);
        assert_eq!(rows[0].rotation, 90.0, "270 + 180 wraps to 90");
        assert_eq!(rows[0].correction, 180.0);
        assert_eq!(rows[1].rotation, 270.0, "-90 normalised");
        assert_eq!(rows[1].layer, "Bottom");
        assert!(cpl_csv(&rows).starts_with("Designator,Mid X,Mid Y,Layer,Rotation\nU2,10.0000mm,20.0000mm,Top,90\n"));
    }

    #[test]
    fn store_zip_is_readable() {
        let dir = std::env::temp_dir().join(format!("someda-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let z = dir.join("t.zip");
        write_store_zip(&z, &[("a.txt".into(), b"hello".to_vec()), ("b/c.gbr".into(), b"G04*".to_vec())]).unwrap();
        let bytes = std::fs::read(&z).unwrap();
        assert_eq!(&bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        assert!(bytes.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]));
        assert_eq!(crc32(b"hello"), 0x3610_a686);
        std::fs::remove_dir_all(&dir).ok();
    }
}
