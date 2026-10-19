//! Footprints and 3D models the local KiCad libraries lack, fetched from
//! KiCad's own library repositories on GitLab into the user's data
//! directory. Nothing is guessed: a footprint is fetched by its exact
//! "Lib:Name", a model by the exact path the footprint references.
//!
//! LCSC/EasyEDA conversion (parts outside the KiCad libraries) is not done
//! here; the Claude Code plugin's `fetch-libs.py` covers it with Python.

use std::path::{Path, PathBuf};

const KICAD_FP_RAW: &str = "https://gitlab.com/kicad/libraries/kicad-footprints/-/raw/master/{lib}.pretty/{name}.kicad_mod";
const KICAD_3D_RAW: &str = "https://gitlab.com/kicad/libraries/kicad-packages3D/-/raw/master/{rel}";

/// `<data>/footprints` and `<data>/3dmodels`, with `<data>` =
/// `SOMEDA_DATA_DIR`, else `~/.local/share/someda` (Unix) or
/// `%LOCALAPPDATA%\someda` (Windows).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SOMEDA_DATA_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(l).join("someda");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("someda")
}

pub fn footprint_dir() -> PathBuf {
    data_dir().join("footprints")
}

pub fn model_dir() -> PathBuf {
    data_dir().join("3dmodels")
}

/// Make the fetched footprints visible to this process's own library
/// lookups (`footprint_lib::search_dirs` reads `SOMEDA_FOOTPRINT_DIRS`).
pub fn register_env() {
    let dir = footprint_dir();
    let cur = std::env::var("SOMEDA_FOOTPRINT_DIRS").unwrap_or_default();
    let d = dir.display().to_string();
    if !cur.split(if cfg!(windows) { ';' } else { ':' }).any(|p| p == d) {
        let joined = if cur.is_empty() { d } else { format!("{}{}{}", cur, if cfg!(windows) { ';' } else { ':' }, d) };
        std::env::set_var("SOMEDA_FOOTPRINT_DIRS", joined);
    }
    std::env::set_var("SOMEDA_FETCHED_DIR", model_dir());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Present,
    Fetched(PathBuf),
    Missing(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// (footprint "Lib:Name", outcome)
    pub footprints: Vec<(String, Outcome)>,
    /// (model path relative to the 3D library, outcome)
    pub models: Vec<(String, Outcome)>,
}

impl FetchReport {
    pub fn fetched(&self) -> usize {
        self.footprints.iter().chain(self.models.iter()).filter(|(_, o)| matches!(o, Outcome::Fetched(_))).count()
    }
    pub fn missing(&self) -> Vec<String> {
        self.footprints.iter().chain(self.models.iter()).filter_map(|(n, o)| if let Outcome::Missing(why) = o { Some(format!("{}: {}", n, why)) } else { None }).collect()
    }
    pub fn summary(&self) -> String {
        let fp_fetched = self.footprints.iter().filter(|(_, o)| matches!(o, Outcome::Fetched(_))).count();
        let m_fetched = self.models.iter().filter(|(_, o)| matches!(o, Outcome::Fetched(_))).count();
        let missing = self.missing();
        format!("{} footprint(s) and {} 3D model(s) fetched from KiCad's libraries{}", fp_fetched, m_fetched, if missing.is_empty() { String::new() } else { format!("; still missing: {}", missing.join(", ")) })
    }
}

/// Fetch every footprint in the list that does not resolve locally, then
/// every 3D model those footprints reference that no model directory has.
pub fn fetch_for(footprints: &[String], system_model_dir: Option<&Path>) -> FetchReport {
    register_env();
    let mut report = FetchReport::default();
    for fp in footprints {
        let outcome = if crate::footprint_lib::resolve(fp).is_some() { Outcome::Present } else { fetch_footprint(fp) };
        report.footprints.push((fp.clone(), outcome));
    }
    for fp in footprints {
        let Some(path) = crate::footprint_lib::resolve(fp) else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for rel in models_referenced(&text) {
            if report.models.iter().any(|(r, _)| r == &rel) {
                continue;
            }
            let in_system = system_model_dir.map(|d| d.join(&rel).exists()).unwrap_or(false);
            let local = model_dir().join(&rel);
            let outcome = if in_system || local.exists() { Outcome::Present } else { fetch_model(&rel) };
            report.models.push((rel, outcome));
        }
    }
    report
}

/// The `Lib.3dshapes/Name.ext` paths a footprint references through a `*_3DMODEL_DIR` variable.
pub fn models_referenced(kicad_mod: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in kicad_mod.lines() {
        let Some(i) = line.find("(model \"") else { continue };
        let rest = &line[i + 8..];
        let Some(j) = rest.find('"') else { continue };
        let path = &rest[..j];
        if let Some(k) = path.find("3DMODEL_DIR}") {
            let rel = path[k + "3DMODEL_DIR}".len()..].trim_start_matches('/').to_string();
            if !rel.is_empty() && !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

fn fetch_footprint(lib_id: &str) -> Outcome {
    let Some((lib, name)) = lib_id.split_once(':') else { return Outcome::Missing("not Lib:Name".into()) };
    if lib.contains("..") || name.contains("..") || lib.contains('/') || name.contains('/') {
        return Outcome::Missing("invalid name".into());
    }
    let url = KICAD_FP_RAW.replace("{lib}", lib).replace("{name}", name);
    let dest = footprint_dir().join(format!("{}.pretty", lib)).join(format!("{}.kicad_mod", name));
    match download(&url, &dest) {
        Ok(()) => match std::fs::read_to_string(&dest).ok().and_then(|t| crate::footprint_lib::parse_kicad_mod(&t).ok()) {
            Some(_) => Outcome::Fetched(dest),
            None => {
                let _ = std::fs::remove_file(&dest);
                Outcome::Missing("downloaded file is not a KiCad footprint".into())
            }
        },
        Err(e) => Outcome::Missing(e),
    }
}

fn fetch_model(rel: &str) -> Outcome {
    if rel.contains("..") {
        return Outcome::Missing("invalid path".into());
    }
    let url = KICAD_3D_RAW.replace("{rel}", rel);
    let dest = model_dir().join(rel);
    match download(&url, &dest) {
        Ok(()) => Outcome::Fetched(dest),
        Err(e) => Outcome::Missing(e),
    }
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let resp = ureq::get(url).set("User-Agent", "someda-app/0.8 (+https://github.com/kabidey/someda)").timeout(std::time::Duration::from_secs(120)).call().map_err(|e| match e {
        ureq::Error::Status(404, _) => "not in KiCad's library repository".to_string(),
        other => other.to_string(),
    })?;
    let tmp = dest.with_extension("part");
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    std::io::copy(&mut resp.into_reader(), &mut f).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_references() {
        let t = "(footprint \"X\" (model \"${KICAD10_3DMODEL_DIR}/Resistor_SMD.3dshapes/R_0603_1608Metric.step\" (offset)))\n(model \"${KICAD8_3DMODEL_DIR}/A.3dshapes/B.wrl\")";
        assert_eq!(models_referenced(t), vec!["Resistor_SMD.3dshapes/R_0603_1608Metric.step".to_string(), "A.3dshapes/B.wrl".to_string()]);
        assert!(matches!(fetch_footprint("bad"), Outcome::Missing(_)));
        assert!(matches!(fetch_footprint("../x:y"), Outcome::Missing(_)));
    }

    /// Network: fetches a real footprint and its model into a temp data dir.
    #[test]
    fn fetches_from_kicad_gitlab_when_allowed() {
        if std::env::var("SOMEDA_NET_TESTS").is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("someda-fetch-{}", std::process::id()));
        std::env::set_var("SOMEDA_DATA_DIR", &dir);
        std::env::set_var("SOMEDA_FOOTPRINT_DIRS", dir.join("footprints"));
        let r = fetch_for(&["Resistor_SMD:R_0603_1608Metric".to_string()], Some(Path::new("/nonexistent")));
        assert!(matches!(r.footprints[0].1, Outcome::Fetched(_) | Outcome::Present), "{:?}", r);
        assert!(r.models.iter().any(|(_, o)| matches!(o, Outcome::Fetched(_))), "{:?}", r);
        let _ = std::fs::remove_dir_all(dir);
    }
}
