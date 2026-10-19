//! The interview: the model proposes, the engine pins down.
//!
//! The model never gets to declare a plan complete. It answers in JSON —
//! either pinned questions (each with concrete options and a recommended
//! one) or a plan. Every plan is checked against what the engine can verify
//! on this machine: the contract, the installed symbol libraries (pin
//! numbers and names), the installed footprints and the pads they carry.
//! What the engine can fix alone it fixes and says so; what needs a choice
//! becomes a question with the real options; what the model got wrong goes
//! back to the model with the facts (the pin tables) until it is right or a
//! round limit is hit, at which point the user decides.

use crate::orchestrator::{validate_plan, CircuitPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where the interview stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the first description.
    Describe,
    /// A model call is in flight.
    Thinking,
    /// Questions wait for the user.
    Questions,
    /// A plan passed every check; the user reviews it and starts the build.
    Review,
}

/// One pinned question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub options: Vec<String>,
    /// Index into `options` the model or engine recommends.
    #[serde(default)]
    pub recommended: Option<usize>,
    /// Why it matters (one line).
    #[serde(default)]
    pub why: String,
    /// The user may type something else (only when the engine can verify it).
    #[serde(default)]
    pub allow_other: bool,
}

/// What the user picked.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    User(String),
    /// Questions the model or the engine asked, with the answers given.
    Asked(Vec<Question>, Vec<Answer>),
    /// A note from the engine (what it fixed, what it checked).
    Engine(String),
}

/// What the engine found wrong with a plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
    /// `validate_plan` rejected it (structural: unknown references, duplicates).
    Invalid(String),
    /// The symbol is not installed; candidates (may be empty).
    SymbolMissing { reference: String, lib_id: String, candidates: Vec<String> },
    /// The footprint is not installed; candidates (may be empty).
    FootprintMissing { reference: String, footprint: String, candidates: Vec<String> },
    /// A net names a pin the symbol does not have.
    PinNotOnSymbol { reference: String, pin: String, lib_id: String, pins: String },
    /// A net names a pad the footprint does not have.
    PinNotOnFootprint { reference: String, pin: String, footprint: String, pads: String },
    /// A power_in pin of an IC is on no net.
    PowerPinUnconnected { reference: String, pin: String, name: String },
}

impl Issue {
    pub fn reference(&self) -> &str {
        match self {
            Issue::Invalid(_) => "",
            Issue::SymbolMissing { reference, .. }
            | Issue::FootprintMissing { reference, .. }
            | Issue::PinNotOnSymbol { reference, .. }
            | Issue::PinNotOnFootprint { reference, .. }
            | Issue::PowerPinUnconnected { reference, .. } => reference,
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Issue::Invalid(m) => m.clone(),
            Issue::SymbolMissing { reference, lib_id, candidates } => format!("{}: symbol \"{}\" is not in the installed KiCad libraries{}", reference, lib_id, if candidates.is_empty() { String::new() } else { format!("; close: {}", candidates.join(", ")) }),
            Issue::FootprintMissing { reference, footprint, candidates } => format!("{}: footprint \"{}\" is not installed{}", reference, footprint, if candidates.is_empty() { String::new() } else { format!("; close: {}", candidates.join(", ")) }),
            Issue::PinNotOnSymbol { reference, pin, lib_id, pins } => format!("{}:{} is not a pin of {} (pins: {})", reference, pin, lib_id, pins),
            Issue::PinNotOnFootprint { reference, pin, footprint, pads } => format!("{}:{} is not a pad of {} (pads: {})", reference, pin, footprint, pads),
            Issue::PowerPinUnconnected { reference, pin, name } => format!("{}:{} ({}) is a power pin on no net", reference, pin, name),
        }
    }
}

/// The engine's verdict on a plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanCheck {
    pub issues: Vec<Issue>,
    /// What the engine changed by itself (symbol/footprint substitutions).
    pub fixes: Vec<String>,
    /// Pin maps per reference, for display and for the model's next round.
    pub pin_maps: BTreeMap<String, String>,
}

impl PlanCheck {
    pub fn ok(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Interview {
    pub phase: Phase,
    pub turns: Vec<Turn>,
    pub pending: Vec<Question>,
    /// Current selections for `pending` (option index or free text).
    pub selections: Vec<(Option<usize>, String)>,
    pub plan: Option<CircuitPlan>,
    pub check: Option<PlanCheck>,
    /// Model calls made so far (the limit is `MAX_MODEL_ROUNDS`).
    pub model_rounds: usize,
    /// Question batches the model may still ask before it must plan.
    pub question_rounds_left: usize,
    pub error: Option<String>,
}

pub const MAX_MODEL_ROUNDS: usize = 8;
pub const MAX_QUESTION_ROUNDS: usize = 3;

impl Default for Interview {
    fn default() -> Self {
        Interview { phase: Phase::Describe, turns: Vec::new(), pending: Vec::new(), selections: Vec::new(), plan: None, check: None, model_rounds: 0, question_rounds_left: MAX_QUESTION_ROUNDS, error: None }
    }
}

/// A request the window must send to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCall {
    pub system: String,
    pub user: String,
}

impl Interview {
    /// The user's first description. Returns the model call to make.
    pub fn describe(&mut self, text: &str) -> ModelCall {
        self.turns.push(Turn::User(text.trim().to_string()));
        self.phase = Phase::Thinking;
        self.model_rounds += 1;
        self.call()
    }

    /// The model's raw reply: questions or a plan.
    pub fn model_replied(&mut self, raw: Result<String, String>) -> Option<ModelCall> {
        let raw = match raw {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                self.phase = if self.plan.is_some() { Phase::Review } else { Phase::Describe };
                return None;
            }
        };
        let json = match crate::gateway::Gateway::extract_json(&raw) {
            Ok(j) => j,
            Err(e) => return self.retry_or_fail(format!("The model did not answer in JSON ({}). Reply with JSON only.", e)),
        };
        if let Some(qs) = json.get("questions").and_then(|q| q.as_array()) {
            let mut questions: Vec<Question> = qs.iter().filter_map(|q| serde_json::from_value(q.clone()).ok()).collect();
            questions.retain(|q| !q.text.trim().is_empty() && q.options.len() >= 2);
            questions.truncate(6);
            if questions.is_empty() || self.question_rounds_left == 0 {
                return self.retry_or_fail("Enough is known. Produce the plan now, as {\"plan\": {...}} only.".into());
            }
            self.question_rounds_left -= 1;
            self.selections = questions.iter().map(|q| (q.recommended.filter(|r| *r < q.options.len()), String::new())).collect();
            self.pending = questions;
            self.phase = Phase::Questions;
            return None;
        }
        let plan_value = json.get("plan").cloned().unwrap_or(json);
        let plan: CircuitPlan = match serde_json::from_value(plan_value) {
            Ok(p) => p,
            Err(e) => return self.retry_or_fail(format!("The plan does not match the contract: {}. Fix it and reply with {{\"plan\": {{...}}}} only.", e)),
        };
        self.accept_plan(plan)
    }

    /// Run the engine checks on a plan; fix what can be fixed, ask the user
    /// what needs a choice, send the rest back to the model.
    pub fn accept_plan(&mut self, plan: CircuitPlan) -> Option<ModelCall> {
        let (plan, check) = check_plan(plan);
        for f in &check.fixes {
            self.turns.push(Turn::Engine(format!("fixed: {}", f)));
        }
        self.plan = Some(plan);
        // Questions the user must answer: missing parts with candidates.
        let mut qs: Vec<Question> = Vec::new();
        for i in &check.issues {
            match i {
                Issue::SymbolMissing { reference, lib_id, candidates } if !candidates.is_empty() => qs.push(Question {
                    id: format!("symbol:{}", reference),
                    text: format!("{}: the symbol \"{}\" is not in the KiCad libraries. Which part is it?", reference, lib_id),
                    options: candidates.clone(),
                    recommended: Some(0),
                    why: "The pinout comes from the library symbol; a wrong symbol is a wrong pinout.".into(),
                    allow_other: true,
                }),
                Issue::FootprintMissing { reference, footprint, candidates } if !candidates.is_empty() => qs.push(Question {
                    id: format!("footprint:{}", reference),
                    text: format!("{}: the footprint \"{}\" is not installed. Which one?", reference, footprint),
                    options: candidates.clone(),
                    recommended: Some(0),
                    why: "The copper is made from the footprint; nothing is guessed.".into(),
                    allow_other: true,
                }),
                _ => {}
            }
        }
        let model_fixable: Vec<&Issue> = check.issues.iter().filter(|i| !matches!(i, Issue::SymbolMissing { candidates, .. } | Issue::FootprintMissing { candidates, .. } if !candidates.is_empty())).collect();
        if !qs.is_empty() {
            self.selections = qs.iter().map(|q| (q.recommended, String::new())).collect();
            self.pending = qs;
            self.check = Some(check);
            self.phase = Phase::Questions;
            return None;
        }
        if !model_fixable.is_empty() {
            let facts = model_fixable.iter().map(|i| format!("- {}", i.describe())).collect::<Vec<_>>().join("\n");
            let maps = check.pin_maps.iter().map(|(r, m)| format!("- {}: {}", r, m)).collect::<Vec<_>>().join("\n");
            let msg = format!("The engine checked the plan against the installed KiCad libraries and found:\n{}\n\nPin tables from the libraries (use these pin NUMBERS):\n{}\n\nReturn the corrected plan as {{\"plan\": {{...}}}} only.", facts, maps);
            self.check = Some(check);
            return self.retry_or_fail(msg);
        }
        self.turns.push(Turn::Engine(format!("checked: {} parts, every symbol, footprint and pin verified against the installed libraries", self.plan.as_ref().map(|p| p.components.len()).unwrap_or(0))));
        self.check = Some(check);
        self.phase = Phase::Review;
        None
    }

    /// Record the user's selection for pending question `i`.
    pub fn select(&mut self, i: usize, option: Option<usize>, other: String) {
        if let Some(s) = self.selections.get_mut(i) {
            *s = (option, other);
        }
    }

    pub fn answers_complete(&self) -> bool {
        self.pending.iter().zip(&self.selections).all(|(q, (opt, other))| opt.is_some() || (q.allow_other && !other.trim().is_empty()))
    }

    /// Submit the answers: engine questions are applied to the plan directly;
    /// model questions go back to the model with the transcript.
    pub fn submit(&mut self) -> Option<ModelCall> {
        if !self.answers_complete() {
            return None;
        }
        let answers: Vec<Answer> = self
            .pending
            .iter()
            .zip(&self.selections)
            .map(|(q, (opt, other))| Answer { id: q.id.clone(), text: opt.and_then(|o| q.options.get(o).cloned()).unwrap_or_else(|| other.trim().to_string()) })
            .collect();
        let engine_questions = self.pending.iter().any(|q| q.id.starts_with("symbol:") || q.id.starts_with("footprint:"));
        self.turns.push(Turn::Asked(self.pending.clone(), answers.clone()));
        self.pending.clear();
        self.selections.clear();
        if engine_questions {
            let mut plan = self.plan.clone().unwrap_or_else(|| serde_json::from_str("{\"name\":\"\",\"description\":\"\",\"components\":[],\"nets\":[]}").unwrap());
            for a in &answers {
                if let Some(r) = a.id.strip_prefix("symbol:") {
                    if let Some(c) = plan.components.iter_mut().find(|c| c.reference == r) {
                        c.lib_id = a.text.clone();
                    }
                } else if let Some(r) = a.id.strip_prefix("footprint:") {
                    if let Some(c) = plan.components.iter_mut().find(|c| c.reference == r) {
                        c.footprint = Some(a.text.clone());
                    }
                }
            }
            return self.accept_plan(plan);
        }
        self.phase = Phase::Thinking;
        self.model_rounds += 1;
        Some(self.call())
    }

    fn retry_or_fail(&mut self, correction: String) -> Option<ModelCall> {
        if self.model_rounds >= MAX_MODEL_ROUNDS {
            self.error = Some(format!("The model could not produce a plan the engine accepts after {} rounds. Last problem: {}", self.model_rounds, correction));
            self.phase = if self.plan.is_some() { Phase::Review } else { Phase::Describe };
            return None;
        }
        self.turns.push(Turn::Engine(correction));
        self.phase = Phase::Thinking;
        self.model_rounds += 1;
        Some(self.call())
    }

    /// The model call for the current transcript.
    fn call(&self) -> ModelCall {
        let mut user = String::new();
        for t in &self.turns {
            match t {
                Turn::User(d) => {
                    user.push_str("The user wants: ");
                    user.push_str(d);
                    user.push('\n');
                }
                Turn::Asked(qs, ans) => {
                    for (q, a) in qs.iter().zip(ans) {
                        user.push_str(&format!("Q: {}\nA: {}\n", q.text, a.text));
                    }
                }
                Turn::Engine(e) => {
                    user.push_str("Engine: ");
                    user.push_str(e);
                    user.push('\n');
                }
            }
        }
        if let Some(p) = &self.plan {
            if let Ok(js) = serde_json::to_string(p) {
                user.push_str("Current plan JSON:\n");
                user.push_str(&js);
                user.push('\n');
            }
        }
        user.push_str(if self.question_rounds_left == 0 || self.plan.is_some() { "\nReply with {\"plan\": {...}} only." } else { "\nReply with {\"questions\": [...]} if something that changes the design is still open, otherwise {\"plan\": {...}}. JSON only." });
        ModelCall { system: SYSTEM_PROMPT.to_string(), user }
    }
}

/// The engine's checks on a plan. Returns the plan with the engine's own
/// substitutions applied and the verdict.
pub fn check_plan(mut plan: CircuitPlan) -> (CircuitPlan, PlanCheck) {
    let mut check = PlanCheck::default();
    if let Err(e) = validate_plan(&plan) {
        check.issues.push(Issue::Invalid(e.to_string()));
    }
    for c in plan.components.iter_mut() {
        // symbol: exact, else one unambiguous candidate (NE555 + DIP → NE555P), else ask
        if !super::symbols::exists(&c.lib_id) {
            let cands = super::symbols::closest(&c.lib_id, 6);
            let pick = pick_by_package(&cands, c.footprint.as_deref());
            match pick {
                Some(p) => {
                    check.fixes.push(format!("{}: symbol \"{}\" → \"{}\" (installed library)", c.reference, c.lib_id, p));
                    c.lib_id = p;
                }
                None => check.issues.push(Issue::SymbolMissing { reference: c.reference.clone(), lib_id: c.lib_id.clone(), candidates: cands }),
            }
        }
        // footprint: exact, else the closest library footprint (as the engine's repair round would), else ask
        match c.footprint.clone() {
            Some(fp) if crate::footprint_lib::resolve(&fp).is_some() => {}
            Some(fp) => match crate::footprint_lib::closest(&fp) {
                Some(found) => {
                    check.fixes.push(format!("{}: footprint \"{}\" → \"{}\"", c.reference, fp, found));
                    c.footprint = Some(found);
                }
                None => check.issues.push(Issue::FootprintMissing { reference: c.reference.clone(), footprint: fp, candidates: vec![] }),
            },
            None => {
                // the symbol's default footprint, when it has one
                match super::symbols::load(&c.lib_id).ok().and_then(|s| s.footprint).filter(|f| crate::footprint_lib::resolve(f).is_some()) {
                    Some(f) => {
                        check.fixes.push(format!("{}: footprint from the symbol: \"{}\"", c.reference, f));
                        c.footprint = Some(f);
                    }
                    None => check.issues.push(Issue::FootprintMissing { reference: c.reference.clone(), footprint: "(none given)".into(), candidates: vec![] }),
                }
            }
        }
    }
    // pins against symbols and footprints
    for c in &plan.components {
        let sym = super::symbols::load(&c.lib_id).ok();
        let fp = c.footprint.as_deref().and_then(|f| crate::footprint_lib::load(f).ok());
        if let Some(s) = &sym {
            check.pin_maps.insert(c.reference.clone(), format!("{}: {}", s.lib_id, super::symbols::pin_map_line(s)));
        }
        let used: Vec<String> = plan.nets.iter().flat_map(|n| n.pins.iter()).filter_map(|p| p.split_once(':')).filter(|(r, _)| *r == c.reference).map(|(_, pin)| pin.to_string()).collect();
        if let Some(s) = &sym {
            for pin in &used {
                if s.pin(pin).is_none() {
                    check.issues.push(Issue::PinNotOnSymbol { reference: c.reference.clone(), pin: pin.clone(), lib_id: s.lib_id.clone(), pins: super::symbols::pin_map_line(s) });
                }
            }
            if c.reference.starts_with('U') {
                for p in s.power_pins() {
                    if !used.contains(&p.number) {
                        check.issues.push(Issue::PowerPinUnconnected { reference: c.reference.clone(), pin: p.number.clone(), name: p.name.clone() });
                    }
                }
            }
        }
        if let Some(f) = &fp {
            let pads: Vec<String> = f.pads.iter().map(|p| p.number.clone()).collect();
            for pin in &used {
                if !pads.iter().any(|p| p == pin) {
                    check.issues.push(Issue::PinNotOnFootprint { reference: c.reference.clone(), pin: pin.clone(), footprint: c.footprint.clone().unwrap_or_default(), pads: pads.join(" ") });
                }
            }
        }
    }
    (plan, check)
}

/// Among symbol candidates, the one whose default footprint matches the
/// plan's package family (DIP / SOIC / TSSOP / QFN / SOT), when exactly one does.
fn pick_by_package(cands: &[String], footprint: Option<&str>) -> Option<String> {
    if cands.len() == 1 {
        return cands.first().cloned();
    }
    let fp = footprint?.to_ascii_uppercase();
    let family = ["DIP", "SOIC", "TSSOP", "SSOP", "QFN", "SOT-23", "SOT-223", "TO-92", "TO-220", "MSOP", "VSSOP"].iter().find(|f| fp.contains(*f))?;
    let fam = |s: &str| {
        let u = s.to_ascii_uppercase();
        if *family == "SOIC" { u.contains("SOIC") || u.contains("SO-") || u.contains("_SO") } else { u.contains(family) }
    };
    let matching: Vec<&String> = cands.iter().filter(|c| super::symbols::load(c).ok().and_then(|s| s.footprint).map(|f| fam(&f)).unwrap_or(false)).collect();
    if matching.len() == 1 {
        Some(matching[0].clone())
    } else {
        None
    }
}

/// The interview prompt, written from the plan contract: intent, never geometry.
pub const SYSTEM_PROMPT: &str = r#"You are the circuit planner for someda, an EDA engine that places, routes and verifies boards by itself. You write the PLAN; you never write coordinates, rotations or wiring geometry.

You answer with JSON only, in one of two shapes:
1. {"questions": [{"id": "q1", "text": "...", "options": ["...", "..."], "recommended": 0, "why": "..."}]}
   Ask only what changes the design and cannot be decided from good electronics practice: supply source and voltage, which connectors and on which board edge, interfaces and their speeds, board size or layer count when it matters, a part choice between real alternatives. 2 to 5 questions per round, each with 2 to 5 concrete options and a recommended one. Never ask the user to confirm something you can decide; never ask about pinouts.
2. {"plan": {...}} — the plan, when nothing that changes the design is open.

The plan contract:
{
  "name": "short name", "description": "one line",
  "layers": 2 | 4 | 6,
  "board": {"width_mm": 50, "height_mm": 40}            (optional; otherwise the engine budgets it),
  "components": [{
    "reference": "U1", "value": "NE555", "lib_id": "Timer:NE555P", "footprint": "Package_DIP:DIP-8_W7.62mm",
    "position": "center" | "top_left" | ... | "left_edge" | "right_edge" | "top_edge" | "bottom_edge"  (optional intent),
    "edge_at": 0.0..1.0 (optional, edges only), "edge_overhang_mm": 0.8 (optional, protruding shells),
    "near": "U1" (optional: hug this part, decoupling capacitors always), "mpn": "...", "lcsc": "C..." (optional),
    "power_w": 0.25 (optional dissipation)
  }],
  "nets": [{"name": "VCC", "pins": ["U1:8", "C2:1"], "freq_hz": 1e6 (optional), "role": "power|ground|clock|rf|highspeed|analog|digital" (optional), "termination": "series|parallel|on-chip" (optional)}],
  "planes": [{"net": "GND", "layer": "In1.Cu"}] (optional, 4/6 layers), "pairs": [{"p": "USB_DP", "n": "USB_DM", "z_diff": 90}] (optional)
}

Rules the engine enforces (it will send the plan back if you break them):
- lib_id is a KiCad library symbol as installed ("Device:R", "Device:C", "Device:LED", "Timer:NE555P", "Connector_Generic:Conn_01x02", "Regulator_Linear:AMS1117-3.3", "MCU_ST_STM32F1:STM32F103C8Tx"); footprint is a KiCad library footprint ("Resistor_SMD:R_0603_1608Metric", "Package_DIP:DIP-8_W7.62mm", "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical").
- Pins are "REF:NUMBER" with the symbol's pin NUMBER, never a pin name. The engine will give you the library's pin table (number and name) for every part; use it.
- Every power pin of every IC is on a net; every IC gets a 100 nF decoupling capacitor with "near" set to it; connectors get an edge position; the ground net is named GND.
- Real parts with real values (E12 resistors, standard capacitors); declare freq_hz and role on clocks, RF and high-speed nets; declare differential pairs in "pairs".
- No coordinates, no rotations, no x_mm/y_mm: the engine owns geometry."#;

#[cfg(test)]
mod tests {
    use super::*;

    fn have_libs() -> bool {
        super::super::symbols::resolve("Device:R").is_some() && crate::footprint_lib::resolve("Resistor_SMD:R_0603_1608Metric").is_some()
    }

    const PLAN: &str = r#"{"plan": {"name":"t","description":"t","layers":2,
      "components":[
        {"reference":"U1","value":"NE555","lib_id":"Timer:NE555","footprint":"Package_DIP:DIP-8_W7.62mm","position":"center"},
        {"reference":"R1","value":"10k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metri"},
        {"reference":"C1","value":"100nF","lib_id":"Device:C","footprint":"Capacitor_SMD:C_0603_1608Metric","near":"U1"}],
      "nets":[{"name":"VCC","pins":["U1:8","U1:4","R1:1","C1:1"]},{"name":"GND","pins":["U1:1","C1:2"]},{"name":"DIS","pins":["U1:7","R1:2"]}]}}"#;

    #[test]
    fn a_plan_with_fixable_names_is_fixed_by_the_engine_and_reviewed() {
        if !have_libs() {
            return;
        }
        let mut iv = Interview::default();
        let call = iv.describe("a 555 blinker");
        assert!(call.system.contains("JSON only"));
        assert_eq!(iv.phase, Phase::Thinking);
        let next = iv.model_replied(Ok(PLAN.into()));
        assert!(next.is_none(), "{:?}", next);
        assert_eq!(iv.phase, Phase::Review, "{:?} {:?}", iv.check, iv.error);
        let p = iv.plan.as_ref().unwrap();
        assert_eq!(p.components[0].lib_id, "Timer:NE555P");
        assert_eq!(p.components[1].footprint.as_deref(), Some("Resistor_SMD:R_0603_1608Metric"));
        assert!(iv.check.as_ref().unwrap().pin_maps.get("U1").unwrap().contains("7 DIS"));
    }

    #[test]
    fn a_wrong_pin_goes_back_to_the_model_with_the_pin_table() {
        if !have_libs() {
            return;
        }
        let mut iv = Interview::default();
        iv.describe("a 555 blinker");
        let bad = PLAN.replace("\"U1:7\"", "\"U1:9\"");
        let next = iv.model_replied(Ok(bad)).expect("a correction round");
        assert!(next.user.contains("U1:9 is not a pin"));
        assert!(next.user.contains("7 DIS"));
        assert_eq!(iv.phase, Phase::Thinking);
    }

    #[test]
    fn questions_are_pinned_and_answers_feed_the_next_call() {
        let mut iv = Interview::default();
        iv.describe("a thing");
        let qs = r#"{"questions":[{"id":"supply","text":"Supply?","options":["USB 5 V","9 V battery"],"recommended":0,"why":"sets the regulator"}]}"#;
        assert!(iv.model_replied(Ok(qs.into())).is_none());
        assert_eq!(iv.phase, Phase::Questions);
        assert!(iv.answers_complete(), "recommended option counts as an answer");
        iv.select(0, Some(1), String::new());
        let call = iv.submit().expect("model call");
        assert!(call.user.contains("A: 9 V battery"));
        assert_eq!(iv.question_rounds_left, MAX_QUESTION_ROUNDS - 1);
    }

    #[test]
    fn non_json_replies_are_retried_until_the_round_limit() {
        let mut iv = Interview::default();
        iv.describe("x");
        for _ in 0..MAX_MODEL_ROUNDS {
            if iv.model_replied(Ok("sorry, no".into())).is_none() {
                break;
            }
        }
        assert!(iv.error.is_some());
        assert_eq!(iv.phase, Phase::Describe);
    }
}
