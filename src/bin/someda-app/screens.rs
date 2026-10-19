//! The interview, build, results and edit screens.

use eframe::egui;
use someda::app::interview::{Phase, Turn};
use someda::app::state::{AppState, Busy, Event, Screen};

/// Events the screen produced this frame.
pub type Pending = Vec<Event>;

pub fn interview(ui: &mut egui::Ui, st: &AppState, description: &mut String) -> Pending {
    let mut out = Pending::new();
    let iv = &st.interview;
    egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 8.0).show(ui, |ui| {
        if iv.phase == Phase::Describe && iv.turns.is_empty() {
            ui.label("What should the board do? Say what it is for, what powers it, what it connects to. The model asks about anything that changes the design; the engine checks every part against the installed KiCad libraries.");
            ui.add_space(6.0);
            ui.add(egui::TextEdit::multiline(description).desired_rows(4).desired_width(f32::INFINITY).hint_text("e.g. a 555 timer blinking an LED at about 1 Hz, 5 V in on a 2-pin header"));
            let can = st.busy == Busy::None && !description.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Ask the model ▶")).clicked() {
                out.push(Event::Describe(description.clone()));
            }
            return;
        }
        // transcript
        for t in &iv.turns {
            match t {
                Turn::User(d) => {
                    ui.label(egui::RichText::new("You").strong());
                    ui.label(d);
                }
                Turn::Asked(qs, ans) => {
                    for (q, a) in qs.iter().zip(ans) {
                        ui.label(egui::RichText::new(&q.text).strong());
                        ui.label(format!("→ {}", a.text));
                    }
                }
                Turn::Engine(e) => {
                    ui.label(egui::RichText::new(format!("engine: {}", e)).italics().small());
                }
            }
            ui.add_space(4.0);
        }
        if let Some(e) = &iv.error {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 120), e);
        }
        match iv.phase {
            Phase::Thinking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("The model is thinking…");
                });
            }
            Phase::Questions => {
                ui.separator();
                for (i, q) in iv.pending.iter().enumerate() {
                    ui.label(egui::RichText::new(&q.text).strong());
                    if !q.why.is_empty() {
                        ui.small(&q.why);
                    }
                    let (sel, other) = iv.selections.get(i).cloned().unwrap_or((None, String::new()));
                    for (oi, opt) in q.options.iter().enumerate() {
                        let label = if q.recommended == Some(oi) { format!("{}  (recommended)", opt) } else { opt.clone() };
                        if ui.radio(sel == Some(oi), label).clicked() {
                            out.push(Event::Select { question: i, option: Some(oi), other: String::new() });
                        }
                    }
                    if q.allow_other {
                        let mut txt = other.clone();
                        ui.horizontal(|ui| {
                            ui.radio(sel.is_none() && !other.trim().is_empty(), "other:");
                            if ui.add(egui::TextEdit::singleline(&mut txt).desired_width(360.0)).changed() {
                                out.push(Event::Select { question: i, option: None, other: txt.clone() });
                            }
                        });
                    }
                    ui.add_space(6.0);
                }
                let can = st.busy == Busy::None && iv.answers_complete();
                if ui.add_enabled(can, egui::Button::new("Continue ▶")).clicked() {
                    out.push(Event::SubmitAnswers);
                }
            }
            Phase::Review => {
                ui.separator();
                if let Some(plan) = &iv.plan {
                    ui.heading(&plan.name);
                    ui.label(&plan.description);
                    ui.label(format!("{} parts · {} nets · {} copper layers", plan.components.len(), plan.nets.len(), plan.layers.unwrap_or(2)));
                    egui::Grid::new("review-parts").striped(true).num_columns(4).spacing([12.0, 4.0]).show(ui, |ui| {
                        for h in ["Ref", "Value", "Symbol", "Footprint"] {
                            ui.label(egui::RichText::new(h).strong());
                        }
                        ui.end_row();
                        for c in &plan.components {
                            ui.label(&c.reference);
                            ui.label(&c.value);
                            ui.label(&c.lib_id);
                            ui.label(c.footprint.clone().unwrap_or_default());
                            ui.end_row();
                        }
                    });
                    if let Some(check) = &iv.check {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("Pin maps from the KiCad libraries").strong());
                        for (r, m) in &check.pin_maps {
                            ui.small(format!("{}: {}", r, m));
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Nets").strong());
                    for n in &plan.nets {
                        ui.small(format!("{}: {}", n.name, n.pins.join(" ")));
                    }
                }
                ui.add_space(8.0);
                if ui.add_enabled(st.busy == Busy::None, egui::Button::new("Build ▶")).clicked() {
                    out.push(Event::StartBuild);
                }
            }
            Phase::Describe => {
                if ui.button("Start over").clicked() {
                    out.push(Event::NewDesign);
                }
            }
        }
    });
    out
}

pub fn build(ui: &mut egui::Ui, st: &AppState) -> Pending {
    let out = Pending::new();
    ui.horizontal(|ui| {
        if st.busy == Busy::Building {
            ui.spinner();
        }
        ui.label(format!("Version {} — {} lines", st.version, st.log.len()));
    });
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for l in &st.log {
            ui.monospace(l);
        }
    });
    out
}

pub fn results(ui: &mut egui::Ui, st: &AppState) -> Pending {
    let mut out = Pending::new();
    let Some(summary) = &st.summary else {
        ui.label(&st.status);
        if ui.button("Back to the design").clicked() {
            out.push(Event::Back);
        }
        return out;
    };
    let dir = st.project.as_ref().map(|p| p.build_dir(st.version));
    let outputs = dir.as_ref().map(|d| someda::app::runner::outputs(d)).unwrap_or_default();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.heading(if summary.honest_done { "All six gates passed" } else { "NOT DONE" });
        ui.horizontal_wrapped(|ui| {
            for (g, ok) in &summary.gates {
                ui.label(egui::RichText::new(format!("{} {}", if *ok { "✓" } else { "✗" }, g)).color(if *ok { egui::Color32::from_rgb(120, 200, 120) } else { egui::Color32::from_rgb(230, 120, 120) }));
            }
        });
        if let Some(png) = &outputs.iso_render {
            let uri = format!("file://{}", png.display());
            ui.add(egui::Image::new(uri).max_width(720.0).corner_radius(6.0));
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if let Some(d) = &dir {
                if ui.button("Open folder").clicked() {
                    open_path(d);
                }
            }
            if let Some(p) = &outputs.project {
                if ui.button("Open in KiCad").clicked() {
                    open_path(p);
                }
            }
            if let Some(z) = &outputs.jlcpcb_zip {
                if ui.button("JLCPCB package").clicked() {
                    open_path(z.parent().unwrap_or(z));
                }
            }
            if ui.button("Change the design…").clicked() {
                out.push(Event::EditRequested);
            }
            if ui.button("New design").clicked() {
                out.push(Event::NewDesign);
            }
        });
        ui.separator();
        egui::Grid::new("stages").striped(true).num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
            for r in &summary.stages {
                ui.label(if r.ok { "✓" } else { "✗" });
                ui.label(format!("[{}] {}", r.stage, r.detail));
                ui.end_row();
            }
        });
        if !summary.repairs.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("The engine fixed by itself").strong());
            for r in &summary.repairs {
                ui.small(format!("repair: {}", r));
            }
        }
        if !summary.suggestions.is_empty() {
            ui.label(egui::RichText::new("Only you can fix").strong());
            for r in &summary.suggestions {
                ui.small(format!("suggest: {}", r));
            }
        }
        if !summary.warnings.is_empty() {
            ui.label(egui::RichText::new("Warnings (real, read them)").strong());
            for w in &summary.warnings {
                ui.small(w);
            }
        }
        if !summary.errors.is_empty() {
            ui.label(egui::RichText::new("Errors").strong());
            for w in &summary.errors {
                ui.small(w);
            }
        }
        if let Some(code) = st.build_exit {
            ui.small(format!("engine exit code {}", code));
        }
    });
    out
}

pub fn edit(ui: &mut egui::Ui, st: &AppState, prompt: &mut String) -> Pending {
    let mut out = Pending::new();
    match (&st.proposal, &st.edit_interview) {
        (Some(_), _) => {
            ui.label(egui::RichText::new("Proposed changes").strong());
            if st.proposal_diff.is_empty() {
                ui.label("No difference from the current plan.");
            }
            for l in &st.proposal_diff {
                ui.monospace(l);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!st.proposal_diff.is_empty() && st.busy == Busy::None, egui::Button::new("Apply and rebuild ▶")).clicked() {
                    out.push(Event::ConfirmEdit);
                }
                if ui.button("Discard").clicked() {
                    out.push(Event::DiscardEdit);
                }
            });
        }
        (None, Some(iv)) if iv.phase == Phase::Questions => {
            // the engine needs a choice (e.g. a part the libraries lack)
            for (i, q) in iv.pending.iter().enumerate() {
                ui.label(egui::RichText::new(&q.text).strong());
                let (sel, other) = iv.selections.get(i).cloned().unwrap_or((None, String::new()));
                for (oi, opt) in q.options.iter().enumerate() {
                    if ui.radio(sel == Some(oi), opt).clicked() {
                        out.push(Event::Select { question: i, option: Some(oi), other: String::new() });
                    }
                }
                if q.allow_other {
                    let mut txt = other.clone();
                    ui.horizontal(|ui| {
                        ui.radio(sel.is_none() && !other.trim().is_empty(), "other:");
                        if ui.add(egui::TextEdit::singleline(&mut txt).desired_width(360.0)).changed() {
                            out.push(Event::Select { question: i, option: None, other: txt.clone() });
                        }
                    });
                }
            }
            if ui.add_enabled(iv.answers_complete(), egui::Button::new("Continue ▶")).clicked() {
                out.push(Event::SubmitAnswers);
            }
        }
        _ => {
            ui.label("What should change? The model gets the plan as built and the last result, proposes a revised plan, and you confirm before anything is rebuilt.");
            ui.add(egui::TextEdit::multiline(prompt).desired_rows(3).desired_width(f32::INFINITY).hint_text("e.g. make the LED blink twice as fast and add a reverse-polarity diode"));
            ui.horizontal(|ui| {
                let can = st.busy == Busy::None && !prompt.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new("Propose ▶")).clicked() {
                    out.push(Event::EditPrompt(prompt.clone()));
                }
                if st.busy == Busy::Thinking {
                    ui.spinner();
                }
                if ui.button("Back").clicked() {
                    out.push(Event::Back);
                }
            });
            if let Some(e) = st.edit_interview.as_ref().and_then(|iv| iv.error.clone()) {
                ui.colored_label(egui::Color32::from_rgb(230, 120, 120), e);
            }
        }
    }
    out
}

/// Open a file or folder with the platform's default handler.
pub fn open_path(p: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![p.display().to_string()]);
    #[cfg(windows)]
    let cmd = ("cmd", vec!["/C".to_string(), "start".to_string(), String::new(), p.display().to_string()]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", vec![p.display().to_string()]);
    let _ = std::process::Command::new(cmd.0).args(cmd.1).spawn();
}

#[allow(dead_code)]
pub fn is_results(s: Screen) -> bool {
    s == Screen::Results
}
