//! What a build needs beyond the app: the someda CLI, kicad-cli, KiCad's
//! footprint, symbol and 3D model libraries. Detection on every platform;
//! installation of KiCad through the official installers (Linux apt is the
//! path exercised on the development machine; the Windows NSIS and macOS
//! dmg flows are implemented from KiCad's published download URLs and the
//! installer's documented switches but were not executed there).

use std::path::{Path, PathBuf};
use std::process::Command;

/// KiCad stable version the installer paths below point at
/// (www.kicad.org/download, 2026-10-19).
pub const KICAD_VERSION: &str = "10.0.7";
pub const KICAD_WINDOWS_URL: &str = "https://kicad-downloads.s3.cern.ch/windows/stable/kicad-10.0.7-x86_64.exe";
pub const KICAD_MACOS_URL: &str = "https://kicad-downloads.s3.cern.ch/osx/stable/kicad-unified-universal-10.0.7.dmg";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Found(String),
    Missing(String),
}

impl Status {
    pub fn ok(&self) -> bool {
        matches!(self, Status::Found(_))
    }
    pub fn text(&self) -> &str {
        match self {
            Status::Found(s) | Status::Missing(s) => s,
        }
    }
}

/// One dependency check.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    /// Resolved path when found (binary or directory).
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.status.ok())
    }
    pub fn get(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.name == name)
    }
    pub fn kicad_cli(&self) -> Option<PathBuf> {
        self.get("kicad-cli").and_then(|c| c.path.clone())
    }
    pub fn someda_cli(&self) -> Option<PathBuf> {
        self.get("someda").and_then(|c| c.path.clone())
    }
}

/// Run every check. `config_kicad_cli` / `config_someda` are user overrides.
pub fn detect(config_kicad_cli: Option<&str>, config_someda: Option<&str>) -> Report {
    let mut checks = Vec::new();
    // 1. the engine CLI: next to this executable, an override, or on PATH
    let someda = config_someda
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| sibling_binary("someda"))
        .or_else(|| which("someda"));
    checks.push(match &someda {
        Some(p) => Check { name: "someda", status: Status::Found(format!("engine {}", p.display())), path: Some(p.clone()) },
        None => Check { name: "someda", status: Status::Missing("the someda engine (CLI) was not found next to the app or on PATH".into()), path: None },
    });
    // 2. kicad-cli
    let kc = config_kicad_cli
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| which("kicad-cli"))
        .or_else(|| kicad_cli_candidates().into_iter().find(|p| p.is_file()));
    checks.push(match &kc {
        Some(p) => {
            let v = Command::new(p).arg("version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
            Check { name: "kicad-cli", status: Status::Found(format!("kicad-cli {} ({})", v.unwrap_or_else(|| "?".into()), p.display())), path: Some(p.clone()) }
        }
        None => Check { name: "kicad-cli", status: Status::Missing(format!("KiCad {} with kicad-cli is not installed", KICAD_VERSION)), path: None },
    });
    // 3. libraries
    for (name, dirs, probe) in [
        ("footprints", footprint_dirs(), "Resistor_SMD.pretty"),
        ("symbols", symbol_dirs(), "Device.kicad_sym"),
        ("3D models", model_dirs(), "Resistor_SMD.3dshapes"),
    ] {
        let found = dirs.into_iter().find(|d| d.join(probe).exists());
        checks.push(match found {
            Some(d) => Check { name, status: Status::Found(format!("{} ({})", name, d.display())), path: Some(d) },
            None => Check { name, status: Status::Missing(format!("KiCad {} library not found (installed with KiCad)", name)), path: None },
        });
    }
    Report { checks }
}

fn sibling_binary(name: &str) -> Option<PathBuf> {
    if let Some(b) = std::env::var_os("SOMEDA_BIN") {
        let p = PathBuf::from(b);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bin = if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() };
    // next to the app; one level up only for cargo's target/<profile>/examples/ layout
    let mut candidates = vec![dir.join(&bin)];
    if dir.file_name().and_then(|n| n.to_str()) == Some("examples") {
        if let Some(p) = dir.parent() {
            candidates.push(p.join(&bin));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// `which`, without a dependency.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) { vec![format!("{}.exe", name), name.to_string()] } else { vec![name.to_string()] };
    for dir in std::env::split_paths(&path) {
        for n in &names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Where KiCad installs kicad-cli when it is not on PATH.
pub fn kicad_cli_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        for root in ["C:\\Program Files\\KiCad", "C:\\Program Files (x86)\\KiCad"] {
            for ver in ["10.0", "9.0", "8.0"] {
                v.push(PathBuf::from(format!("{}\\{}\\bin\\kicad-cli.exe", root, ver)));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            for ver in ["10.0", "9.0", "8.0"] {
                v.push(PathBuf::from(local.clone()).join("Programs").join("KiCad").join(ver).join("bin").join("kicad-cli.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"));
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join("Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push(PathBuf::from("/usr/bin/kicad-cli"));
        v.push(PathBuf::from("/usr/local/bin/kicad-cli"));
        v.push(PathBuf::from("/var/lib/flatpak/exports/bin/org.kicad.KiCad"));
    }
    v
}

pub fn footprint_dirs() -> Vec<PathBuf> {
    crate::footprint_lib::search_dirs()
}

pub fn symbol_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for var in ["KICAD10_SYMBOL_DIR", "KICAD9_SYMBOL_DIR", "KICAD8_SYMBOL_DIR"] {
        if let Some(d) = std::env::var_os(var) {
            v.push(PathBuf::from(d));
        }
    }
    v.extend(
        [
            "/usr/share/kicad/symbols",
            "/usr/local/share/kicad/symbols",
            "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols",
            "C:\\Program Files\\KiCad\\10.0\\share\\kicad\\symbols",
            "C:\\Program Files\\KiCad\\9.0\\share\\kicad\\symbols",
            "C:\\Program Files\\KiCad\\8.0\\share\\kicad\\symbols",
        ]
        .iter()
        .map(PathBuf::from),
    );
    v
}

pub fn model_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for var in ["KICAD10_3DMODEL_DIR", "KICAD9_3DMODEL_DIR", "KICAD8_3DMODEL_DIR"] {
        if let Some(d) = std::env::var_os(var) {
            v.push(PathBuf::from(d));
        }
    }
    v.extend(
        [
            "/usr/share/kicad/3dmodels",
            "/usr/local/share/kicad/3dmodels",
            "/Applications/KiCad/KiCad.app/Contents/SharedSupport/3dmodels",
            "C:\\Program Files\\KiCad\\10.0\\share\\kicad\\3dmodels",
            "C:\\Program Files\\KiCad\\9.0\\share\\kicad\\3dmodels",
            "C:\\Program Files\\KiCad\\8.0\\share\\kicad\\3dmodels",
        ]
        .iter()
        .map(PathBuf::from),
    );
    v
}

/// The install plan for this platform, as the app explains it before asking
/// for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    /// `apt-get install kicad kicad-footprints kicad-symbols kicad-packages3d` (needs root/sudo).
    Apt,
    /// Download the NSIS installer and run it silently (`/allusers /S`, needs an elevated prompt).
    WindowsNsis { url: String },
    /// Download the dmg, attach it, copy KiCad.app into /Applications.
    MacDmg { url: String },
    /// No automatic path (other package managers): the download page.
    Manual { url: String },
}

pub fn install_plan() -> InstallPlan {
    if cfg!(windows) {
        InstallPlan::WindowsNsis { url: KICAD_WINDOWS_URL.into() }
    } else if cfg!(target_os = "macos") {
        InstallPlan::MacDmg { url: KICAD_MACOS_URL.into() }
    } else if which("apt-get").is_some() {
        InstallPlan::Apt
    } else {
        InstallPlan::Manual { url: "https://www.kicad.org/download/".into() }
    }
}

impl InstallPlan {
    pub fn describe(&self) -> String {
        match self {
            InstallPlan::Apt => "Install KiCad with apt (kicad, kicad-footprints, kicad-symbols, kicad-packages3d — about 1.5 GB). Needs administrator rights.".into(),
            InstallPlan::WindowsNsis { url } => format!("Download the KiCad {} installer ({}) and run it silently for all users (needs administrator rights; about 1 GB, a few minutes).", KICAD_VERSION, url),
            InstallPlan::MacDmg { url } => format!("Download KiCad {} ({}), open the disk image and copy KiCad.app into /Applications (about 1 GB).", KICAD_VERSION, url),
            InstallPlan::Manual { url } => format!("No installer for this system; get KiCad from {} and click Re-check.", url),
        }
    }
}

/// Run the install plan, blocking; returns the log. The caller runs it on a
/// worker thread and re-detects afterwards.
pub fn install(plan: &InstallPlan, download_dir: &Path) -> Result<String, String> {
    match plan {
        InstallPlan::Apt => {
            let sudo = if unix_is_root() { vec![] } else { vec!["sudo".to_string(), "-n".to_string()] };
            let mut args: Vec<String> = sudo;
            args.extend(["apt-get", "install", "-y", "-q", "kicad", "kicad-footprints", "kicad-symbols", "kicad-packages3d"].iter().map(|s| s.to_string()));
            run_logged(&args, &[("DEBIAN_FRONTEND", "noninteractive")])
        }
        InstallPlan::WindowsNsis { url } => {
            let exe = download(url, download_dir)?;
            run_logged(&[exe.display().to_string(), "/allusers".into(), "/S".into()], &[])
        }
        InstallPlan::MacDmg { url } => {
            let dmg = download(url, download_dir)?;
            let mount = download_dir.join("kicad-dmg");
            let mut log = run_logged(&["hdiutil".into(), "attach".into(), "-nobrowse".into(), "-mountpoint".into(), mount.display().to_string(), dmg.display().to_string()], &[])?;
            let app = mount.join("KiCad").join("KiCad.app");
            let src = if app.exists() { app } else { mount.join("KiCad.app") };
            log.push_str(&run_logged(&["ditto".into(), src.display().to_string(), "/Applications/KiCad/KiCad.app".into()], &[])?);
            log.push_str(&run_logged(&["hdiutil".into(), "detach".into(), mount.display().to_string()], &[]).unwrap_or_default());
            Ok(log)
        }
        InstallPlan::Manual { url } => Err(format!("no automatic installer on this system; download KiCad from {}", url)),
    }
}

fn unix_is_root() -> bool {
    #[cfg(unix)]
    {
        std::fs::metadata("/proc/self").map(|_| unsafe_geteuid() == 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn unsafe_geteuid() -> u32 {
    // /proc/self/status "Uid:" line avoids an FFI dependency
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(2).and_then(|v| v.parse().ok())))
        .unwrap_or(1)
}

fn run_logged(args: &[String], env: &[(&str, &str)]) -> Result<String, String> {
    let (cmd, rest) = args.split_first().ok_or("empty command")?;
    let mut c = Command::new(cmd);
    c.args(rest);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().map_err(|e| format!("{}: {}", cmd, e))?;
    let log = format!("$ {}\n{}{}", args.join(" "), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(log)
    } else {
        Err(format!("{}\nexit status {}", log, out.status))
    }
}

/// Download `url` into `dir`, returning the file path.
pub fn download(url: &str, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = url.rsplit('/').next().unwrap_or("download.bin");
    let path = dir.join(name);
    let resp = ureq::get(url).timeout(std::time::Duration::from_secs(3600)).call().map_err(|e| format!("download {}: {}", url, e))?;
    let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    std::io::copy(&mut resp.into_reader(), &mut file).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_tools_on_this_machine_when_present() {
        let r = detect(None, None);
        assert_eq!(r.checks.len(), 5);
        if which("kicad-cli").is_some() {
            assert!(r.get("kicad-cli").unwrap().status.ok());
        }
        let plan = install_plan();
        assert!(!plan.describe().is_empty());
    }

    #[test]
    fn install_urls_carry_the_pinned_version() {
        assert!(KICAD_WINDOWS_URL.contains(KICAD_VERSION) && KICAD_WINDOWS_URL.ends_with(".exe"));
        assert!(KICAD_MACOS_URL.contains(KICAD_VERSION) && KICAD_MACOS_URL.ends_with(".dmg"));
    }
}
