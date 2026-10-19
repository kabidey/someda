//! The BoardPlan contract — someda's port of the harness JSON forms.
//!
//! The harness defines TWO agent-facing schemas:
//!
//! **Form 1 "plan"** — placement/routing/unroute intent:
//!   {"v":2,"plan":{"place":[...],"rows":[...],"groups":[...],
//!    "apart":[...],"outline":{...},"route":true,"unroute":true}}
//!
//! **Form 5 "bringup"** — the whole-board build, phases in fixed order:
//!   {"v":2,"bringup":{"summary":"...","layers":2,
//!    "stackup":[{"layer":"B.Cu","role":"ground_plane"}],
//!    "net_classes":[{"name":"power","trace_width_mm":0.5,"clearance_mm":0.2,"nets":[...]}],
//!    "planes":[{"net":"GND","layer":"B.Cu"}],
//!    "placement":{...plan schema...},
//!    "route":true,
//!    "headline":"..."}}
//!
//! Rules ported verbatim from the harness:
//! - The app executes the bringup phases in the correct order:
//!   layers → stackup roles → net classes → placement → planes → route → final checks
//! - "route":true runs the autorouter AFTER placement in the same plan
//! - "unroute":true removes existing copper BEFORE any other work
//! - Routing is never implied by "finish/complete the board" — only an explicit
//!   route flag that the user's words back
//! - Never output coordinates; the deterministic solver owns geometry
//!
//! Parsing is strict: unknown keys are rejected (the harness drops
//! unbacked work, we reject it loudly — honest-done).

use crate::placer::{self, PlacementIntent};
use serde::Deserialize;

/// The top-level agent output: exactly one of plan or bringup.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOutput {
    pub v: u8,
    #[serde(default)]
    pub plan: Option<BoardPlan>,
    #[serde(default)]
    pub bringup: Option<Bringup>,
}

/// Form 1: the placement/routing plan.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BoardPlan {
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub place: Vec<PlaceEntry>,
    #[serde(default)]
    pub rows: Vec<RowEntry>,
    #[serde(default)]
    pub groups: Vec<GroupEntry>,
    #[serde(default)]
    pub apart: Vec<ApartEntry>,
    #[serde(default)]
    pub outline: Option<OutlineEntry>,
    #[serde(default)]
    pub route: RouteFlag,
    #[serde(default)]
    pub unroute: UnrouteFlag,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    All,
    Selected,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PlaceEntry {
    pub reference: String,
    pub pos: Option<String>,
    pub rot: Option<i32>,
    #[serde(default)]
    pub side_back: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RowEntry {
    pub references: Vec<String>,
    #[serde(rename = "axis")]
    pub axis: String,
    #[serde(default)]
    pub equal: bool,
    #[serde(default)]
    pub center: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GroupEntry {
    pub anchor: String,
    pub members: Vec<String>,
    #[serde(rename = "max_mm")]
    pub max_mm: f64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ApartEntry {
    pub a: String,
    pub b: String,
    #[serde(rename = "min_mm")]
    pub min_mm: f64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OutlineEntry {
    pub shape: String,
    #[serde(default)]
    pub size: Option<Vec<f64>>,
    #[serde(default)]
    pub diameter: Option<f64>,
    #[serde(default)]
    pub corner_radius: Option<f64>,
    #[serde(default)]
    pub sides: Option<u32>,
    #[serde(default)]
    pub rotation: Option<f64>,
}

/// route: bool | {"nets": [...]} — port of the harness dual form.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(untagged)]
pub enum RouteFlag {
    #[default]
    Off,
    On(bool),
    Nets(RouteNets),
}

impl RouteFlag {
    pub fn is_on(&self) -> bool {
        match self {
            RouteFlag::Off => false,
            RouteFlag::On(b) => *b,
            RouteFlag::Nets(n) => n.nets.is_some() || true,
        }
    }

    pub fn nets(&self) -> Option<&Vec<String>> {
        match self {
            RouteFlag::Nets(n) => n.nets.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RouteNets {
    pub nets: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(untagged)]
pub enum UnrouteFlag {
    #[default]
    Off,
    On(bool),
    Scope(UnrouteScope),
}

impl UnrouteFlag {
    pub fn is_on(&self) -> bool {
        match self {
            UnrouteFlag::Off => false,
            UnrouteFlag::On(b) => *b,
            UnrouteFlag::Scope(_) => true,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct UnrouteScope {
    pub nets: Option<Vec<String>>,
    pub refs: Option<Vec<String>>,
}

/// Form 5: bringup — the whole-board build.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bringup {
    pub summary: String,
    #[serde(default = "default_layers")]
    pub layers: u32,
    #[serde(default)]
    pub stackup: Vec<StackupRole>,
    #[serde(default)]
    pub net_classes: Vec<NetClassEntry>,
    #[serde(default)]
    pub planes: Vec<PlaneEntry>,
    #[serde(default)]
    pub placement: Option<BoardPlan>,
    #[serde(default)]
    pub route: RouteFlag,
    pub headline: String,
}

fn default_layers() -> u32 {
    2
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct StackupRole {
    pub layer: String,
    #[serde(rename = "role")]
    pub role: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetClassEntry {
    pub name: String,
    #[serde(rename = "trace_width_mm")]
    pub trace_width_mm: f64,
    #[serde(rename = "clearance_mm")]
    pub clearance_mm: f64,
    pub nets: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PlaneEntry {
    pub net: String,
    pub layer: String,
}

/// Errors that reject unbacked work (the harness drops it; we say why).
#[derive(Debug)]
pub enum ContractError {
    NotJson(String),
    UnknownVersion(u8),
    EmptyPlan,
    UnknownPos(String),
    UnknownAxis(String),
    UnknownRole(String),
    BadLayers(u32),
    NetNotFound(String),
    RefNotFound(String),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractError::NotJson(e) => write!(f, "output does not match the contract: {}", e),
            ContractError::UnknownVersion(v) => write!(f, "unsupported version v{}", v),
            ContractError::EmptyPlan => write!(f, "plan and bringup are both empty"),
            ContractError::UnknownPos(p) => write!(f, "unknown pos '{}'", p),
            ContractError::UnknownAxis(a) => write!(f, "unknown axis '{}'", a),
            ContractError::UnknownRole(r) => write!(f, "unknown stackup role '{}'", r),
            ContractError::BadLayers(n) => write!(f, "layer count {} must be even and >= 2", n),
            ContractError::NetNotFound(n) => write!(f, "net '{}' does not exist on the board", n),
            ContractError::RefNotFound(r) => write!(f, "reference '{}' does not exist on the board", r),
        }
    }
}

impl AgentOutput {
    /// Parse and validate an agent output against the contract.
    pub fn parse(json: &str) -> Result<AgentOutput, ContractError> {
        let out: AgentOutput = serde_json::from_str(json)
            .map_err(|e| ContractError::NotJson(e.to_string()))?;
        if out.v != 2 {
            return Err(ContractError::UnknownVersion(out.v));
        }
        if out.plan.is_none() && out.bringup.is_none() {
            return Err(ContractError::EmptyPlan);
        }
        if let Some(p) = &out.plan {
            validate_plan_entries(p)?;
        }
        if let Some(b) = &out.bringup {
            if b.layers < 2 || b.layers % 2 != 0 {
                return Err(ContractError::BadLayers(b.layers));
            }
            for s in &b.stackup {
                if !["signal", "ground_plane", "power_plane", "mixed"].contains(&s.role.as_str()) {
                    return Err(ContractError::UnknownRole(s.role.clone()));
                }
            }
            for nc in &b.net_classes {
                if nc.trace_width_mm <= 0.0 || nc.clearance_mm <= 0.0 {
                    return Err(ContractError::NotJson(format!(
                        "net class '{}' must have positive width and clearance",
                        nc.name
                    )));
                }
            }
            if let Some(p) = &b.placement {
                validate_plan_entries(p)?;
            }
        }
        Ok(out)
    }
}

fn validate_plan_entries(p: &BoardPlan) -> Result<(), ContractError> {
    for place in &p.place {
        if let Some(pos) = &place.pos {
            if placer::Pos::parse(pos).is_none() {
                return Err(ContractError::UnknownPos(pos.clone()));
            }
        }
    }
    for row in &p.rows {
        if row.axis != "horizontal" && row.axis != "vertical" {
            return Err(ContractError::UnknownAxis(row.axis.clone()));
        }
    }
    Ok(())
}

impl BoardPlan {
    /// Convert the plan-form entries into the placer's intent.
    pub fn placement_intent(&self) -> PlacementIntent {
        let mut intent = PlacementIntent::default();
        for p in &self.place {
            intent.places.push(placer::PlaceIntent {
                reference: p.reference.clone(),
                pos: p.pos.as_deref().and_then(placer::Pos::parse),
                rot: p.rot,
                side_back: p.side_back,
                            edge_at: None,
            });
        }
        for g in &self.groups {
            intent.groups.push(placer::GroupIntent {
                anchor: g.anchor.clone(),
                members: g.members.clone(),
                max_mm: g.max_mm,
            });
        }
        for a in &self.apart {
            intent.apart.push(placer::ApartIntent {
                a: a.a.clone(),
                b: a.b.clone(),
                min_mm: a.min_mm,
            });
        }
        for r in &self.rows {
            intent.rows.push(placer::RowIntent {
                references: r.references.clone(),
                axis: if r.axis == "horizontal" {
                    placer::Axis::Horizontal
                } else {
                    placer::Axis::Vertical
                },
                equal: r.equal,
                center: r.center,
            });
        }
        intent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_form() {
        let out = AgentOutput::parse(
            r#"{"v":2,"plan":{
                "place":[{"reference":"U1","pos":"center"}],
                "apart":[{"a":"U1","b":"J1","min_mm":15}],
                "route":true}}"#,
        )
        .unwrap();
        assert!(out.plan.is_some());
        assert!(out.plan.as_ref().unwrap().route.is_on());
    }

    #[test]
    fn parses_bringup_form() {
        let out = AgentOutput::parse(
            r#"{"v":2,"bringup":{
                "summary":"2-layer devkit",
                "layers":2,
                "stackup":[{"layer":"B.Cu","role":"ground_plane"}],
                "net_classes":[{"name":"power","trace_width_mm":0.5,"clearance_mm":0.2,"nets":["VBUS"]}],
                "planes":[{"net":"GND","layer":"B.Cu"}],
                "route":true,
                "headline":"GND pour on B.Cu"}}"#,
        )
        .unwrap();
        let b = out.bringup.unwrap();
        assert_eq!(b.layers, 2);
        assert_eq!(b.net_classes[0].nets[0], "VBUS");
        assert!(b.route.is_on());
    }

    #[test]
    fn rejects_v1() {
        let err = AgentOutput::parse(r#"{"v":1,"plan":{}}"#).unwrap_err();
        assert!(matches!(err, ContractError::UnknownVersion(1)));
    }

    #[test]
    fn rejects_unknown_pos() {
        let err = AgentOutput::parse(
            r#"{"v":2,"plan":{"place":[{"reference":"U1","pos":"sideways-ish"}]}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::UnknownPos(_)));
    }

    #[test]
    fn rejects_empty_output() {
        let err = AgentOutput::parse(r#"{"v":2}"#).unwrap_err();
        assert!(matches!(err, ContractError::EmptyPlan));
    }

    #[test]
    fn rejects_odd_layers() {
        let err = AgentOutput::parse(
            r#"{"v":2,"bringup":{"summary":"x","layers":3,"headline":"y"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::BadLayers(3)));
    }

    #[test]
    fn route_nets_form() {
        let out = AgentOutput::parse(
            r#"{"v":2,"plan":{"route":{"nets":["GND"]}}}"#,
        )
        .unwrap();
        let route = out.plan.unwrap().route;
        assert!(route.is_on());
        assert_eq!(route.nets().map(|n| n[0].clone()), Some("GND".into()));
    }

    #[test]
    fn unroute_scope_form() {
        let out = AgentOutput::parse(
            r#"{"v":2,"plan":{"unroute":{"nets":["GND"],"refs":["U1"]}}}"#,
        )
        .unwrap();
        let u = out.plan.unwrap().unroute;
        assert!(u.is_on());
        match u {
            UnrouteFlag::Scope(s) => {
                assert_eq!(s.nets.as_ref().unwrap()[0], "GND");
                assert_eq!(s.refs.as_ref().unwrap()[0], "U1");
            }
            _ => panic!("expected scope"),
        }
    }
}