//! someda-app: the desktop window. All logic is in `someda::app`; this file
//! draws the current screen, forwards clicks as events, and runs the
//! effects the state machine asks for on worker threads.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod screens;

use eframe::egui;
use someda::app::config::AppConfig;
use someda::app::deps;
use someda::app::providers::{self, Protocol};
use someda::app::state::{AppState, Busy, Effect, Event, Screen};
use std::sync::mpsc::{channel, Receiver, Sender};

struct App {
    state: AppState,
    events: Receiver<Event>,
    tx: Sender<Event>,
    frames: u32,
    screenshot: Option<std::path::PathBuf>,
    shot_requested: bool,
    /// Text being typed (the state machine only sees it when submitted).
    description: String,
    edit_prompt: String,
}

impl App {
    fn new(state: AppState) -> Self {
        let (tx, events) = channel();
        let screenshot = std::env::var_os("SOMEDA_APP_SCREENSHOT").map(std::path::PathBuf::from);
        App { state, events, tx, frames: 0, screenshot, shot_requested: false, description: String::new(), edit_prompt: String::new() }
    }

    fn dispatch(&mut self, ev: Event) {
        let effect = self.state.apply(ev);
        self.run(effect);
    }

    /// Effects run on worker threads; their results come back as events.
    fn run(&mut self, effect: Effect) {
        let tx = self.tx.clone();
        match effect {
            Effect::None => {}
            Effect::SaveConfig => {
                if let Err(e) = self.state.config.save() {
                    self.state.status = format!("Settings not saved: {}", e);
                }
            }
            Effect::ListModels { protocol, base_url, api_key } => {
                std::thread::spawn(move || {
                    let gw = someda::gateway::Gateway::new(protocol, &base_url, &api_key, "");
                    let _ = tx.send(Event::ModelsListed(gw.list_models().map_err(|e| e.to_string())));
                });
            }
            Effect::TestConnection { protocol, base_url, api_key, model } => {
                std::thread::spawn(move || {
                    let gw = someda::gateway::Gateway::new(protocol, &base_url, &api_key, &model);
                    let r = gw.chat("You are a terse assistant.", "Reply with the single word OK.").map_err(|e| e.to_string());
                    let _ = tx.send(Event::ConnectionTested(r));
                });
            }
            Effect::Detect => {
                let (kc, sc) = (self.state.config.kicad_cli.clone(), self.state.config.someda_cli.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(Event::Detected(deps::detect(kc.as_deref(), sc.as_deref())));
                });
            }
            Effect::Install(plan) => {
                let dir = std::env::temp_dir().join("someda-downloads");
                std::thread::spawn(move || {
                    let _ = tx.send(Event::Installed(deps::install(&plan, &dir)));
                });
            }
            Effect::AskModel(call) => {
                let (protocol, base_url, api_key, model) = (self.state.protocol(), self.state.config.base_url.clone(), self.state.config.api_key.clone(), self.state.config.model.clone());
                std::thread::spawn(move || {
                    let gw = someda::gateway::Gateway::new(protocol, &base_url, &api_key, &model);
                    let r = gw.chat_with_temperature(&call.system, &call.user, Some(0.1)).map_err(|e| e.to_string());
                    let _ = tx.send(Event::ModelReplied(r));
                });
            }
            Effect::CheckPlan { plan, custom_symbols } => {
                std::thread::spawn(move || {
                    let r = someda::app::interview::check_plan(plan, &custom_symbols);
                    let _ = tx.send(Event::PlanChecked(Box::new(r)));
                });
            }
            Effect::FetchLibraries { footprints, system_model_dir } => {
                std::thread::spawn(move || {
                    let r = someda::app::libfetch::fetch_for(&footprints, system_model_dir.as_deref());
                    let _ = tx.send(Event::LibrariesFetched(r));
                });
            }
            Effect::SpawnBuild(req) => {
                let rx = someda::app::runner::spawn(req);
                std::thread::spawn(move || {
                    for ev in rx {
                        let _ = tx.send(Event::BuildOutput(ev));
                    }
                });
            }
        }
    }

    fn provider_screen(&mut self, ui: &mut egui::Ui) {
        let mut pending: Vec<Event> = Vec::new();
        ui.label("Which language model writes the circuit plan? The key stays on this computer and is sent only to the provider you pick.");
        ui.add_space(8.0);
        egui::Grid::new("provider-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Provider");
            let current = self.state.provider().map(|p| p.name).unwrap_or("choose…").to_string();
            egui::ComboBox::from_id_salt("provider").selected_text(current).show_ui(ui, |ui| {
                for p in providers::PROVIDERS {
                    if ui.selectable_label(self.state.config.provider == p.id, p.name).clicked() {
                        pending.push(Event::ProviderChosen(p.id.to_string()));
                    }
                }
            });
            ui.end_row();

            ui.label("Endpoint");
            let mut url = self.state.config.base_url.clone();
            if ui.add(egui::TextEdit::singleline(&mut url).desired_width(420.0)).changed() {
                pending.push(Event::BaseUrlEdited(url));
            }
            ui.end_row();

            let local = self.state.provider().map(|p| p.local).unwrap_or(false);
            if !local {
                ui.label("API key");
                ui.horizontal(|ui| {
                    let mut key = self.state.config.api_key.clone();
                    if ui.add(egui::TextEdit::singleline(&mut key).password(true).desired_width(420.0)).changed() {
                        pending.push(Event::KeyEdited(key));
                    }
                    if let Some(p) = self.state.provider() {
                        if !p.key_url.is_empty() {
                            ui.hyperlink_to("get a key", p.key_url);
                        }
                    }
                });
                ui.end_row();
            }

            ui.label("Model");
            ui.horizontal(|ui| {
                if self.state.models.is_empty() {
                    let mut m = self.state.config.model.clone();
                    if ui.add(egui::TextEdit::singleline(&mut m).desired_width(300.0).hint_text("model name")).changed() {
                        pending.push(Event::ModelEdited(m));
                    }
                } else {
                    let shown = if self.state.config.model.is_empty() { "choose…".to_string() } else { self.state.config.model.clone() };
                    egui::ComboBox::from_id_salt("model").selected_text(shown).width(300.0).show_ui(ui, |ui| {
                        for m in &self.state.models {
                            if ui.selectable_label(&self.state.config.model == m, m).clicked() {
                                pending.push(Event::ModelEdited(m.clone()));
                            }
                        }
                    });
                }
                let can = self.state.busy == Busy::None && !self.state.config.base_url.is_empty();
                if ui.add_enabled(can, egui::Button::new("List models")).clicked() {
                    pending.push(Event::ListModels);
                }
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let can_test = self.state.busy == Busy::None && self.state.provider_complete();
            if ui.add_enabled(can_test, egui::Button::new("Test connection")).clicked() {
                pending.push(Event::TestConnection);
            }
            let tested_ok = matches!(self.state.connection, Some(Ok(_)));
            if ui.add_enabled(tested_ok && self.state.busy == Busy::None, egui::Button::new("Continue ▶")).clicked() {
                pending.push(Event::ProviderDone);
            }
            if self.state.busy != Busy::None {
                ui.spinner();
            }
        });
        if self.state.protocol() == Protocol::Anthropic {
            ui.small("Anthropic Messages API; every other provider uses the OpenAI chat-completions protocol.");
        }
        for ev in pending {
            self.dispatch(ev);
        }
    }

    fn dependencies_screen(&mut self, ui: &mut egui::Ui) {
        let mut pending: Vec<Event> = Vec::new();
        ui.label("A build needs the someda engine, KiCad's kicad-cli, and KiCad's footprint, symbol and 3D model libraries.");
        ui.add_space(8.0);
        match &self.state.deps {
            None => {
                if self.state.busy == Busy::None {
                    pending.push(Event::Detect);
                }
            }
            Some(report) => {
                egui::Grid::new("deps-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    for c in &report.checks {
                        ui.label(if c.status.ok() { "✓" } else { "✗" });
                        ui.label(c.status.text());
                        ui.end_row();
                    }
                });
                if let Some(plan) = &self.state.install_plan {
                    ui.add_space(8.0);
                    ui.label(plan.describe());
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let idle = self.state.busy == Busy::None;
            if ui.add_enabled(idle, egui::Button::new("Re-check")).clicked() {
                pending.push(Event::Detect);
            }
            if self.state.install_plan.is_some() && ui.add_enabled(idle, egui::Button::new("Install KiCad")).clicked() {
                pending.push(Event::Install);
            }
            let ok = self.state.deps.as_ref().map(|d| d.all_ok()).unwrap_or(false);
            if ui.add_enabled(ok && idle, egui::Button::new("Continue ▶")).clicked() {
                pending.push(Event::DependenciesDone);
            }
            if !idle {
                ui.spinner();
            }
        });
        if let Some(Err(e)) = &self.state.install_log {
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                ui.monospace(e);
            });
        }
        for ev in pending {
            self.dispatch(ev);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames += 1;
        while let Ok(ev) = self.events.try_recv() {
            self.dispatch(ev);
        }
        egui::Panel::top("title").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.state.screen != Screen::Welcome && ui.button("◀").clicked() {
                    self.dispatch(Event::Back);
                }
                ui.heading(self.state.screen.title());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("someda {}", env!("CARGO_PKG_VERSION")));
                });
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.label(&self.state.status);
        });
        egui::Frame::new().inner_margin(16.0).show(ui, |ui| match self.state.screen {
            Screen::Welcome => {
                ui.heading("Describe a circuit; get a fab-ready KiCad project with every claim checked.");
                ui.add_space(8.0);
                ui.label("The model writes the plan. The engine places, routes, audits, verifies through KiCad, and exports Gerbers, BOM and a JLCPCB package. Six gates decide whether it is done.");
                ui.add_space(16.0);
                if ui.button("Start").clicked() {
                    self.dispatch(Event::Start);
                }
            }
            Screen::Provider => self.provider_screen(ui),
            Screen::Dependencies => self.dependencies_screen(ui),
            Screen::Interview => {
                let evs = screens::interview(ui, &self.state, &mut self.description);
                for ev in evs {
                    self.dispatch(ev);
                }
            }
            Screen::Build => {
                let evs = screens::build(ui, &self.state);
                for ev in evs {
                    self.dispatch(ev);
                }
            }
            Screen::Results => {
                let evs = screens::results(ui, &self.state);
                for ev in evs {
                    self.dispatch(ev);
                }
            }
            Screen::Edit => {
                let evs = screens::edit(ui, &self.state, &mut self.edit_prompt);
                for ev in evs {
                    self.dispatch(ev);
                }
            }
        });
        if self.state.busy != Busy::None {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        // Headless proof that a screen renders: SOMEDA_APP_SCREENSHOT=<png>
        if let Some(path) = self.screenshot.clone() {
            if self.frames >= 4 && !self.shot_requested {
                self.shot_requested = true;
                let ctx = ui.ctx().clone();
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img: std::sync::Arc<egui::ColorImage>| {
                    let (w, h) = (img.size[0] as u32, img.size[1] as u32);
                    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                    for px in &img.pixels {
                        rgba.extend_from_slice(&px.to_array());
                    }
                    if let Err(e) = image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                        eprintln!("someda-app: screenshot failed: {}", e);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                })));
            }
            ui.ctx().request_repaint();
        }
    }
}

fn main() -> eframe::Result {
    someda::app::libfetch::register_env();
    let mut state = AppState::with_config(AppConfig::load());
    // SOMEDA_APP_SCREEN=provider|dependencies: start on that screen (screenshots, tests)
    match std::env::var("SOMEDA_APP_SCREEN").as_deref() {
        Ok("provider") => {
            state.apply(Event::Start);
        }
        Ok("dependencies") => {
            state.go(Screen::Dependencies);
        }
        Ok("interview") => {
            state.go(Screen::Interview);
        }
        Ok("review") => {
            // SOMEDA_APP_PROJECT=<dir>: load its latest plan into the review step
            if let Some(dir) = std::env::var_os("SOMEDA_APP_PROJECT") {
                let dir = std::path::PathBuf::from(dir);
                let v = someda::app::project::Project::latest_version(&dir).unwrap_or(1);
                let p = someda::app::project::Project { dir, version: v };
                if let Ok(plan) = p.load_plan(v) {
                    state.interview.turns.push(someda::app::interview::Turn::User(plan.description.clone()));
                    let _ = state.interview.accept_plan(plan);
                }
            }
            state.go(Screen::Interview);
        }
        Ok("edit") => {
            // SOMEDA_APP_PROJECT=<dir>: show the latest version as a proposal against v1
            if let Some(dir) = std::env::var_os("SOMEDA_APP_PROJECT") {
                let dir = std::path::PathBuf::from(dir);
                let v = someda::app::project::Project::latest_version(&dir).unwrap_or(1);
                let p = someda::app::project::Project { dir, version: v };
                if let (Ok(old), Ok(new)) = (p.load_plan(1), p.load_plan(v)) {
                    state.proposal_diff = someda::app::project::diff(&old, &new);
                    state.proposal = Some(new);
                    state.project = Some(p);
                    state.version = 1;
                    state.status = format!("{} change(s) proposed; confirm to rebuild.", state.proposal_diff.len());
                }
            }
            state.go(Screen::Edit);
        }
        Ok("results") => {
            // SOMEDA_APP_PROJECT=<dir with plan.v1.json + build-v1/>: show its result
            if let Some(dir) = std::env::var_os("SOMEDA_APP_PROJECT") {
                let dir = std::path::PathBuf::from(dir);
                let v = someda::app::project::Project::latest_version(&dir).unwrap_or(1);
                state.project = Some(someda::app::project::Project { dir: dir.clone(), version: v });
                state.version = v;
                if let Ok(log) = std::fs::read_to_string(dir.join(format!("build-v{}", v)).join("build.log")) {
                    state.log = log.lines().map(|l| l.to_string()).collect();
                    state.summary = Some(someda::app::runner::summarize(&state.log));
                }
            }
            state.go(Screen::Results);
        }
        _ => {}
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]).with_title("someda"),
        ..Default::default()
    };
    eframe::run_native(
        "someda",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(App::new(state)))
        }),
    )
}
