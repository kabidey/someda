//! Where a design lives on disk: numbered plan versions and one build
//! directory per version, so every edit is undoable and every result stays.
//!
//! `<projects>/<slug>/plan.v1.json`, `build-v1/`, `plan.v2.json`, `build-v2/`, …

use crate::orchestrator::CircuitPlan;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub dir: PathBuf,
    pub version: u32,
}

impl Project {
    /// Create (or reuse) the project folder for a plan name.
    pub fn create(projects_root: &Path, name: &str) -> Result<Project, String> {
        let slug = slug(name);
        let dir = projects_root.join(if slug.is_empty() { "design".to_string() } else { slug });
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
        let version = Self::latest_version(&dir).unwrap_or(0);
        Ok(Project { dir, version })
    }

    pub fn latest_version(dir: &Path) -> Option<u32> {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.strip_prefix("plan.v")?.strip_suffix(".json")?.parse::<u32>().ok()
            })
            .max()
    }

    /// Save a plan as the next version; returns its path.
    pub fn save_next(&mut self, plan: &CircuitPlan) -> Result<PathBuf, String> {
        self.version += 1;
        let path = self.plan_path(self.version);
        let text = serde_json::to_string_pretty(plan).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("write {}: {}", path.display(), e))?;
        Ok(path)
    }

    pub fn plan_path(&self, version: u32) -> PathBuf {
        self.dir.join(format!("plan.v{}.json", version))
    }

    pub fn build_dir(&self, version: u32) -> PathBuf {
        self.dir.join(format!("build-v{}", version))
    }

    pub fn load_plan(&self, version: u32) -> Result<CircuitPlan, String> {
        let p = self.plan_path(version);
        let text = std::fs::read_to_string(&p).map_err(|e| format!("read {}: {}", p.display(), e))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {}", p.display(), e))
    }

    /// The plan as the engine built it, when the build wrote one.
    pub fn built_plan(&self, version: u32) -> Option<CircuitPlan> {
        let p = self.build_dir(version).join("plan.built.json");
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// Default projects root: `~/someda-projects` (or `SOMEDA_PROJECTS_DIR`).
pub fn default_root() -> PathBuf {
    if let Some(d) = std::env::var_os("SOMEDA_PROJECTS_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("someda-projects")
}

pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').chars().take(48).collect()
}

/// Human-readable difference between two plans (components and nets).
pub fn diff(old: &CircuitPlan, new: &CircuitPlan) -> Vec<String> {
    let mut lines = Vec::new();
    for c in &new.components {
        match old.components.iter().find(|o| o.reference == c.reference) {
            None => lines.push(format!("+ {} {} ({}, {})", c.reference, c.value, c.lib_id, c.footprint.clone().unwrap_or_default())),
            Some(o) => {
                if o.value != c.value || o.lib_id != c.lib_id || o.footprint != c.footprint || o.position != c.position || o.near != c.near {
                    lines.push(format!("~ {}: {} {} {} → {} {} {}", c.reference, o.value, o.lib_id, o.footprint.clone().unwrap_or_default(), c.value, c.lib_id, c.footprint.clone().unwrap_or_default()));
                }
            }
        }
    }
    for o in &old.components {
        if !new.components.iter().any(|c| c.reference == o.reference) {
            lines.push(format!("- {} {}", o.reference, o.value));
        }
    }
    for n in &new.nets {
        match old.nets.iter().find(|o| o.name == n.name) {
            None => lines.push(format!("+ net {}: {}", n.name, n.pins.join(" "))),
            Some(o) => {
                let mut a = o.pins.clone();
                let mut b = n.pins.clone();
                a.sort();
                b.sort();
                if a != b {
                    lines.push(format!("~ net {}: {} → {}", n.name, o.pins.join(" "), n.pins.join(" ")));
                }
            }
        }
    }
    for o in &old.nets {
        if !new.nets.iter().any(|n| n.name == o.name) {
            lines.push(format!("- net {}", o.name));
        }
    }
    if old.layers != new.layers {
        lines.push(format!("~ layers {:?} → {:?}", old.layers, new.layers));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(json: &str) -> CircuitPlan {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn versions_are_numbered_and_diffs_are_readable() {
        let root = std::env::temp_dir().join(format!("someda-proj-{}", std::process::id()));
        let mut p = Project::create(&root, "555 LED blinker!").unwrap();
        assert!(p.dir.ends_with("555-led-blinker"));
        let a = plan(r#"{"name":"a","description":"","components":[{"reference":"R1","value":"10k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metric"}],"nets":[{"name":"VCC","pins":["R1:1"]}]}"#);
        let b = plan(r#"{"name":"a","description":"","components":[{"reference":"R1","value":"22k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metric"},{"reference":"C1","value":"100nF","lib_id":"Device:C","footprint":"Capacitor_SMD:C_0603_1608Metric"}],"nets":[{"name":"VCC","pins":["R1:1","C1:1"]},{"name":"GND","pins":["C1:2"]}]}"#);
        let p1 = p.save_next(&a).unwrap();
        let p2 = p.save_next(&b).unwrap();
        assert!(p1.ends_with("plan.v1.json") && p2.ends_with("plan.v2.json"));
        assert_eq!(Project::latest_version(&p.dir), Some(2));
        assert_eq!(p.load_plan(1).unwrap().components[0].value, "10k");
        let d = diff(&a, &b);
        assert!(d.iter().any(|l| l.starts_with("~ R1: 10k")), "{:?}", d);
        assert!(d.iter().any(|l| l.starts_with("+ C1 100nF")));
        assert!(d.iter().any(|l| l.starts_with("+ net GND")));
        let _ = std::fs::remove_dir_all(root);
    }
}
