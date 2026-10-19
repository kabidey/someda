//! The app as a state machine: which screen is shown, what it knows, and
//! how it reacts to events. No GUI type appears here; the window renders
//! `AppState` and calls `apply` with what happened.

use super::config::AppConfig;
use super::deps::{InstallPlan, Report};
use super::providers::{self, Protocol};

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
        AppState { screen, config, busy: Busy::None, models: Vec::new(), connection: None, deps: None, install_plan: None, install_log: None, status: String::new() }
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
