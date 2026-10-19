//! The app as a state machine: which screen is shown, what it knows, and
//! how it reacts to events. No GUI type appears here; the window renders
//! `AppState` and calls `apply` with what happened.

use super::config::AppConfig;
use super::deps::{InstallPlan, Report};
use super::interview::{Interview, ModelCall, Phase};
use super::project::{self, Project};
use super::providers::{self, Protocol};
use super::runner::{self, BuildEvent, BuildRequest, BuildSummary};
use crate::orchestrator::CircuitPlan;
use std::path::PathBuf;

/// The screens, in the order a first session walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Welcome,
    Provider,
    Dependencies,
    Interview,
    Build,
    Results,
    Edit,
}

impl Screen {
    pub fn title(self) -> &'static str {
        match self {
            Screen::Welcome => "someda",
            Screen::Provider => "Language model",
            Screen::Dependencies => "KiCad and libraries",
            Screen::Interview => "What to build",
            Screen::Build => "Building",
            Screen::Results => "Result",
            Screen::Edit => "Change the design",
        }
    }
}

/// A background job the window is waiting for (it polls a channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    None,
    ListingModels,
    TestingConnection,
    Detecting,
    Installing,
    Thinking,
    Building,
}

/// Things that happen; the window produces them, `apply` consumes them.
#[derive(Debug, Clone)]
pub enum Event {
    Start,
    ProviderChosen(String),
    BaseUrlEdited(String),
    KeyEdited(String),
    ModelEdited(String),
    ListModels,
    ModelsListed(Result<Vec<String>, String>),
    TestConnection,
    ConnectionTested(Result<String, String>),
    ProviderDone,
    Detect,
    Detected(Report),
    Install,
    Installed(Result<String, String>),
    DependenciesDone,
    /// Interview: the user's description, answers, and the model's replies.
    Describe(String),
    Select { question: usize, option: Option<usize>, other: String },
    SubmitAnswers,
    ModelReplied(Result<String, String>),
    /// Review → build.
    StartBuild,
    BuildOutput(BuildEvent),
    /// Results → edit; the edit prompt; confirm the proposed plan.
    EditRequested,
    EditPrompt(String),
    ConfirmEdit,
    DiscardEdit,
    NewDesign,
    Back,
}

/// What the window must start when `apply` returns it.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    None,
    /// Save the config file.
    SaveConfig,
    /// Call the provider's model listing with these settings.
    ListModels { protocol: Protocol, base_url: String, api_key: String },
    /// One chat round trip with these settings.
    TestConnection { protocol: Protocol, base_url: String, api_key: String, model: String },
    /// Run dependency detection.
    Detect,
    /// Run the install plan.
    Install(InstallPlan),
    /// Send this to the model; the reply comes back as `ModelReplied`.
    AskModel(ModelCall),
    /// Start the engine; lines come back as `BuildOutput`.
    SpawnBuild(BuildRequest),
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub screen: Screen,
    pub config: AppConfig,
    pub busy: Busy,
    /// Models the provider listed (empty: type one).
    pub models: Vec<String>,
    /// Last connection test outcome.
    pub connection: Option<Result<String, String>>,
    pub deps: Option<Report>,
    pub install_plan: Option<InstallPlan>,
    pub install_log: Option<Result<String, String>>,
    /// Message shown at the bottom of the current screen.
    pub status: String,
    pub interview: Interview,
    pub project: Option<Project>,
    /// The version being built or shown.
    pub version: u32,
    pub log: Vec<String>,
    pub summary: Option<BuildSummary>,
    pub build_exit: Option<i32>,
    /// Edit flow: the request, the model's proposal and its diff against the current plan.
    pub edit_prompt: String,
    pub proposal: Option<CircuitPlan>,
    pub proposal_diff: Vec<String>,
    pub edit_interview: Option<Interview>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState::with_config(AppConfig::load())
    }
}

impl AppState {
    pub fn with_config(config: AppConfig) -> Self {
        // A saved, complete configuration skips the setup screens.
        let screen = Screen::Welcome;
        AppState {
            screen,
            config,
            busy: Busy::None,
            models: Vec::new(),
            connection: None,
            deps: None,
            install_plan: None,
            install_log: None,
            status: String::new(),
            interview: Interview::default(),
            project: None,
            version: 0,
            log: Vec::new(),
            summary: None,
            build_exit: None,
            edit_prompt: String::new(),
            proposal: None,
            proposal_diff: Vec::new(),
            edit_interview: None,
        }
    }

    fn projects_root(&self) -> PathBuf {
        self.config.projects_dir.as_deref().map(PathBuf::from).unwrap_or_else(project::default_root)
    }

    /// The engine's environment for a build: the key by env only.
    fn model_env(&self) -> Vec<(String, String)> {
        vec![
            ("SOMEDA_API_KEY".into(), self.config.api_key.clone()),
            ("SOMEDA_BASE_URL".into(), self.config.base_url.clone()),
            ("SOMEDA_MODEL".into(), self.config.model.clone()),
        ]
    }

    /// The plan currently shown: the engine's built plan when a build ran, else the interview's.
    pub fn current_plan(&self) -> Option<CircuitPlan> {
        if let (Some(p), v) = (&self.project, self.version) {
            if let Some(b) = p.built_plan(v) {
                return Some(b);
            }
            if let Ok(pl) = p.load_plan(v) {
                return Some(pl);
            }
        }
        self.interview.plan.clone()
    }

    fn after_model_call(&mut self, call: Option<ModelCall>, iv_phase: Phase) -> Effect {
        match call {
            Some(c) => {
                self.busy = Busy::Thinking;
                self.status = "Waiting for the model…".into();
                Effect::AskModel(c)
            }
            None => {
                self.busy = Busy::None;
                self.status = match iv_phase {
                    Phase::Questions => "A few things to pin down.".into(),
                    Phase::Review => "The plan passed every library check. Review it, then build.".into(),
                    Phase::Describe => self.interview.error.clone().unwrap_or_default(),
                    Phase::Thinking => String::new(),
                };
                Effect::None
            }
        }
    }

    fn start_build(&mut self, plan: &CircuitPlan) -> Effect {
        let root = self.projects_root();
        let mut project = match self.project.take() {
            Some(p) => p,
            None => match Project::create(&root, &plan.name) {
                Ok(p) => p,
                Err(e) => {
                    self.status = e;
                    return Effect::None;
                }
            },
        };
        let plan_file = match project.save_next(plan) {
            Ok(p) => p,
            Err(e) => {
                self.status = e;
                self.project = Some(project);
                return Effect::None;
            }
        };
        self.version = project.version;
        let project_dir = project.build_dir(project.version);
        let deps = self.deps.clone().unwrap_or_default();
        let Some(someda_cli) = deps.someda_cli() else {
            self.status = "The someda engine was not found (Dependencies screen).".into();
            self.project = Some(project);
            return Effect::None;
        };
        let req = BuildRequest {
            someda_cli,
            plan_file,
            project_dir,
            description: plan.description.clone(),
            kicad_cli: deps.kicad_cli(),
            footprint_dirs: vec![],
            model_dir: deps.get("3D models").and_then(|c| c.path.clone()),
            env: self.model_env(),
            log_file: Some(project_dir.join("build.log")),
        };
        self.project = Some(project);
        self.log.clear();
        self.summary = None;
        self.build_exit = None;
        self.busy = Busy::Building;
        self.go(Screen::Build);
        self.status = format!("Building version {}…", self.version);
        Effect::SpawnBuild(req)
    }

    pub fn go(&mut self, screen: Screen) {
        self.screen = screen;
        self.status.clear();
    }

    pub fn provider(&self) -> Option<&'static providers::Provider> {
        providers::by_id(&self.config.provider)
    }

    pub fn protocol(&self) -> Protocol {
        self.provider().map(|p| p.protocol).unwrap_or(Protocol::OpenAi)
    }

    /// The provider setup is usable: a URL, a model, and a key unless local.
    pub fn provider_complete(&self) -> bool {
        let local = self.provider().map(|p| p.local).unwrap_or(false);
        !self.config.base_url.trim().is_empty() && !self.config.model.trim().is_empty() && (local || !self.config.api_key.trim().is_empty())
    }

    /// When the edit interview has a checked plan, turn it into a proposal with a diff.
    fn settle_edit(&mut self) {
        if let Some(iv) = &self.edit_interview {
            if iv.phase == Phase::Review {
                if let (Some(new), Some(old)) = (iv.plan.clone(), self.current_plan()) {
                    self.proposal_diff = project::diff(&old, &new);
                    self.proposal = Some(new);
                    self.status = if self.proposal_diff.is_empty() { "The model proposes no change.".into() } else { format!("{} change(s) proposed; confirm to rebuild.", self.proposal_diff.len()) };
                }
            } else if iv.phase == Phase::Describe {
                self.status = iv.error.clone().unwrap_or_default();
            }
        }
    }

    pub fn apply(&mut self, ev: Event) -> Effect {
        match ev {
            Event::Start => {
                self.go(Screen::Provider);
                if self.config.provider.is_empty() {
                    return self.apply(Event::ProviderChosen("anthropic".into()));
                }
                Effect::None
            }
            Event::ProviderChosen(id) => {
                if let Some(p) = providers::by_id(&id) {
                    self.config.provider = id;
                    self.config.base_url = p.base_url.to_string();
                    self.models.clear();
                    self.connection = None;
                    self.config.model.clear();
                    if p.local {
                        self.config.api_key.clear();
                    }
                }
                Effect::None
            }
            Event::BaseUrlEdited(u) => {
                self.config.base_url = u;
                Effect::None
            }
            Event::KeyEdited(k) => {
                self.config.api_key = k;
                self.connection = None;
                Effect::None
            }
            Event::ModelEdited(m) => {
                self.config.model = m;
                self.connection = None;
                Effect::None
            }
            Event::ListModels => {
                self.busy = Busy::ListingModels;
                self.status = "Asking the provider for its models…".into();
                Effect::ListModels { protocol: self.protocol(), base_url: self.config.base_url.clone(), api_key: self.config.api_key.clone() }
            }
            Event::ModelsListed(r) => {
                self.busy = Busy::None;
                match r {
                    Ok(list) => {
                        self.status = format!("{} models listed", list.len());
                        if self.config.model.is_empty() {
                            if let Some(first) = list.first() {
                                self.config.model = first.clone();
                            }
                        }
                        self.models = list;
                    }
                    Err(e) => {
                        self.status = format!("No model list from this provider ({}); type the model name.", e);
                        self.models.clear();
                    }
                }
                Effect::None
            }
            Event::TestConnection => {
                self.busy = Busy::TestingConnection;
                self.status = "Sending one test message…".into();
                Effect::TestConnection { protocol: self.protocol(), base_url: self.config.base_url.clone(), api_key: self.config.api_key.clone(), model: self.config.model.clone() }
            }
            Event::ConnectionTested(r) => {
                self.busy = Busy::None;
                self.status = match &r {
                    Ok(reply) => format!("Connected. The model answered: {}", reply.chars().take(80).collect::<String>()),
                    Err(e) => format!("Not connected: {}", e),
                };
                self.connection = Some(r);
                Effect::None
            }
            Event::ProviderDone => {
                if !self.provider_complete() {
                    self.status = "Choose a provider, enter the key and a model first.".into();
                    return Effect::None;
                }
                self.go(Screen::Dependencies);
                Effect::SaveConfig
            }
            Event::Detect => {
                self.busy = Busy::Detecting;
                self.status = "Looking for KiCad and the libraries…".into();
                Effect::Detect
            }
            Event::Detected(r) => {
                self.busy = Busy::None;
                self.install_plan = if r.all_ok() { None } else { Some(super::deps::install_plan()) };
                self.status = if r.all_ok() { "Everything a build needs is installed.".into() } else { "Something is missing.".into() };
                self.deps = Some(r);
                Effect::None
            }
            Event::Install => match self.install_plan.clone() {
                Some(plan) => {
                    self.busy = Busy::Installing;
                    self.status = "Installing KiCad… this takes a few minutes.".into();
                    Effect::Install(plan)
                }
                None => Effect::None,
            },
            Event::Installed(r) => {
                self.busy = Busy::None;
                self.status = match &r {
                    Ok(_) => "Installed. Checking again…".into(),
                    Err(e) => format!("Install failed: {}", e.lines().last().unwrap_or("")),
                };
                self.install_log = Some(r);
                Effect::Detect
            }
            Event::DependenciesDone => {
                let ok = self.deps.as_ref().map(|d| d.all_ok()).unwrap_or(false);
                if !ok {
                    self.status = "KiCad or its libraries are still missing.".into();
                    return Effect::None;
                }
                self.go(Screen::Interview);
                Effect::SaveConfig
            }
            Event::Describe(text) => {
                if text.trim().is_empty() {
                    return Effect::None;
                }
                self.interview = Interview::default();
                let call = self.interview.describe(&text);
                self.after_model_call(Some(call), Phase::Thinking)
            }
            Event::Select { question, option, other } => {
                match self.edit_interview.as_mut() {
                    Some(iv) => iv.select(question, option, other),
                    None => self.interview.select(question, option, other),
                }
                Effect::None
            }
            Event::SubmitAnswers => {
                if let Some(iv) = self.edit_interview.as_mut() {
                    let call = iv.submit();
                    let phase = iv.phase;
                    let eff = self.after_model_call(call, phase);
                    self.settle_edit();
                    return eff;
                }
                let call = self.interview.submit();
                let phase = self.interview.phase;
                self.after_model_call(call, phase)
            }
            Event::ModelReplied(r) => {
                if let Some(iv) = self.edit_interview.as_mut() {
                    let call = iv.model_replied(r);
                    let phase = iv.phase;
                    let eff = self.after_model_call(call, phase);
                    self.settle_edit();
                    return eff;
                }
                let call = self.interview.model_replied(r);
                let phase = self.interview.phase;
                self.after_model_call(call, phase)
            }
            Event::StartBuild => {
                let Some(plan) = self.interview.plan.clone() else { return Effect::None };
                if self.interview.phase != Phase::Review {
                    return Effect::None;
                }
                self.start_build(&plan)
            }
            Event::BuildOutput(ev) => {
                match ev {
                    BuildEvent::Line(l) => self.log.push(l),
                    BuildEvent::Finished { exit_code } => {
                        self.build_exit = Some(exit_code);
                        self.summary = Some(runner::summarize(&self.log));
                        self.busy = Busy::None;
                        let done = self.summary.as_ref().map(|s| s.honest_done).unwrap_or(false);
                        self.go(Screen::Results);
                        self.status = if done { "All six gates passed.".into() } else { "NOT DONE: a gate failed; the engine says which.".into() };
                    }
                    BuildEvent::Failed(e) => {
                        self.build_exit = Some(-1);
                        self.busy = Busy::None;
                        self.status = format!("The engine did not run: {}", e);
                        self.go(Screen::Results);
                    }
                }
                Effect::None
            }
            Event::EditRequested => {
                self.edit_prompt.clear();
                self.proposal = None;
                self.proposal_diff.clear();
                self.edit_interview = None;
                self.go(Screen::Edit);
                Effect::None
            }
            Event::EditPrompt(text) => {
                self.edit_prompt = text.clone();
                let Some(current) = self.current_plan() else { return Effect::None };
                if text.trim().is_empty() {
                    return Effect::None;
                }
                let mut iv = Interview::default();
                iv.plan = Some(current.clone());
                iv.question_rounds_left = 0;
                let summary = self.summary.as_ref().map(|s| {
                    let mut t: Vec<String> = s.stages.iter().map(|r| format!("{} [{}] {}", if r.ok { "ok" } else { "FAILED" }, r.stage, r.detail)).collect();
                    t.extend(s.warnings.iter().map(|w| format!("warning: {}", w)));
                    t.extend(s.suggestions.iter().map(|w| format!("suggest: {}", w)));
                    t.join("\n")
                }).unwrap_or_default();
                let call = iv.describe(&format!("Change the existing design as follows: {}\n\nLast build result:\n{}", text.trim(), summary));
                self.edit_interview = Some(iv);
                self.after_model_call(Some(call), Phase::Thinking)
            }
            Event::ConfirmEdit => {
                let Some(plan) = self.proposal.take() else { return Effect::None };
                self.proposal_diff.clear();
                self.edit_interview = None;
                self.interview.plan = Some(plan.clone());
                self.interview.phase = Phase::Review;
                self.start_build(&plan)
            }
            Event::DiscardEdit => {
                self.proposal = None;
                self.proposal_diff.clear();
                self.edit_interview = None;
                self.go(Screen::Results);
                Effect::None
            }
            Event::NewDesign => {
                self.interview = Interview::default();
                self.project = None;
                self.version = 0;
                self.log.clear();
                self.summary = None;
                self.go(Screen::Interview);
                Effect::None
            }
            Event::Back => {
                let prev = match self.screen {
                    Screen::Welcome | Screen::Provider => Screen::Welcome,
                    Screen::Dependencies => Screen::Provider,
                    Screen::Interview => Screen::Dependencies,
                    Screen::Build => Screen::Interview,
                    Screen::Results => Screen::Interview,
                    Screen::Edit => Screen::Results,
                };
                self.go(prev);
                Effect::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> AppState {
        AppState::with_config(AppConfig::default())
    }

    #[test]
    fn start_picks_anthropic_by_default_and_needs_key_and_model_to_continue() {
        let mut s = fresh();
        assert_eq!(s.screen, Screen::Welcome);
        assert_eq!(s.apply(Event::Start), Effect::None);
        assert_eq!(s.screen, Screen::Provider);
        assert_eq!(s.config.provider, "anthropic");
        assert_eq!(s.protocol(), Protocol::Anthropic);
        assert_eq!(s.apply(Event::ProviderDone), Effect::None);
        assert_eq!(s.screen, Screen::Provider, "incomplete setup must not advance");
        s.apply(Event::KeyEdited("sk-ant-test".into()));
        s.apply(Event::ModelEdited("claude-sonnet-4-5".into()));
        assert_eq!(s.apply(Event::ProviderDone), Effect::SaveConfig);
        assert_eq!(s.screen, Screen::Dependencies);
    }

    #[test]
    fn local_ollama_needs_no_key_and_model_listing_fills_the_model() {
        let mut s = fresh();
        s.apply(Event::Start);
        s.apply(Event::ProviderChosen("ollama".into()));
        assert_eq!(s.config.base_url, "http://127.0.0.1:11434/v1");
        match s.apply(Event::ListModels) {
            Effect::ListModels { protocol, .. } => assert_eq!(protocol, Protocol::OpenAi),
            e => panic!("{:?}", e),
        }
        s.apply(Event::ModelsListed(Ok(vec!["llama3.1".into(), "qwen2.5".into()])));
        assert_eq!(s.config.model, "llama3.1");
        assert!(s.provider_complete());
    }

    #[test]
    fn interview_to_build_to_results_to_edit() {
        if super::super::symbols::resolve("Device:R").is_none() || crate::footprint_lib::resolve("Resistor_SMD:R_0603_1608Metric").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("someda-state-{}", std::process::id()));
        let mut s = fresh();
        s.config.projects_dir = Some(root.display().to_string());
        s.deps = Some(Report { checks: vec![super::super::deps::Check { name: "someda", status: super::super::deps::Status::Found("x".into()), path: Some(PathBuf::from("/bin/true")) }] });
        s.go(Screen::Interview);
        // description → model call
        match s.apply(Event::Describe("two resistors".into())) {
            Effect::AskModel(c) => assert!(c.user.contains("two resistors")),
            e => panic!("{:?}", e),
        }
        // model asks a question → user answers → next call
        let qs = r#"{"questions":[{"id":"q","text":"Which?","options":["a","b"],"recommended":1}]}"#;
        assert_eq!(s.apply(Event::ModelReplied(Ok(qs.into()))), Effect::None);
        assert_eq!(s.interview.phase, Phase::Questions);
        assert!(matches!(s.apply(Event::SubmitAnswers), Effect::AskModel(_)));
        // model returns a plan with a misspelled footprint → engine fixes → review
        let plan = r#"{"plan":{"name":"two r","description":"two resistors","layers":2,
          "components":[{"reference":"R1","value":"10k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metri"},
                        {"reference":"R2","value":"10k","lib_id":"Device:R","footprint":"Resistor_SMD:R_0603_1608Metric"}],
          "nets":[{"name":"A","pins":["R1:1","R2:1"]},{"name":"GND","pins":["R1:2","R2:2"]}]}}"#;
        assert_eq!(s.apply(Event::ModelReplied(Ok(plan.into()))), Effect::None);
        assert_eq!(s.interview.phase, Phase::Review, "{:?}", s.interview.error);
        // build: the plan is saved as v1 and the engine is spawned
        match s.apply(Event::StartBuild) {
            Effect::SpawnBuild(req) => {
                assert!(req.plan_file.ends_with("plan.v1.json"));
                assert!(req.project_dir.ends_with("build-v1"));
                assert!(req.env.iter().any(|(k, _)| k == "SOMEDA_API_KEY"));
            }
            e => panic!("{:?}", e),
        }
        assert_eq!(s.screen, Screen::Build);
        s.apply(Event::BuildOutput(BuildEvent::Line("  ✓ [route] routed 2 of 2 connections".into())));
        s.apply(Event::BuildOutput(BuildEvent::Line("─── HONEST-DONE: all 6 gates pass ───".into())));
        s.apply(Event::BuildOutput(BuildEvent::Finished { exit_code: 0 }));
        assert_eq!(s.screen, Screen::Results);
        assert!(s.summary.as_ref().unwrap().honest_done);
        // edit: the model proposes a change → diff → confirm → v2 build
        s.apply(Event::EditRequested);
        assert!(matches!(s.apply(Event::EditPrompt("make R1 22k".into())), Effect::AskModel(_)));
        let edited = plan.replace("\"value\":\"10k\",\"lib_id\":\"Device:R\",\"footprint\":\"Resistor_SMD:R_0603_1608Metri\"", "\"value\":\"22k\",\"lib_id\":\"Device:R\",\"footprint\":\"Resistor_SMD:R_0603_1608Metric\"");
        assert_eq!(s.apply(Event::ModelReplied(Ok(edited))), Effect::None);
        assert!(s.proposal.is_some(), "{:?}", s.status);
        assert!(s.proposal_diff.iter().any(|l| l.contains("R1") && l.contains("22k")), "{:?}", s.proposal_diff);
        match s.apply(Event::ConfirmEdit) {
            Effect::SpawnBuild(req) => assert!(req.plan_file.ends_with("plan.v2.json")),
            e => panic!("{:?}", e),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dependencies_gate_the_interview_and_install_rechecks() {
        let mut s = fresh();
        s.go(Screen::Dependencies);
        assert_eq!(s.apply(Event::DependenciesDone), Effect::None);
        assert_eq!(s.screen, Screen::Dependencies);
        let missing = Report { checks: vec![super::super::deps::Check { name: "kicad-cli", status: super::super::deps::Status::Missing("x".into()), path: None }] };
        s.apply(Event::Detected(missing));
        assert!(s.install_plan.is_some());
        assert_eq!(s.apply(Event::Installed(Ok("done".into()))), Effect::Detect);
        let ok = Report { checks: vec![super::super::deps::Check { name: "kicad-cli", status: super::super::deps::Status::Found("x".into()), path: None }] };
        s.apply(Event::Detected(ok));
        assert_eq!(s.apply(Event::DependenciesDone), Effect::SaveConfig);
        assert_eq!(s.screen, Screen::Interview);
    }
}
