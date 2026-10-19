//! Drives the desktop app's state machine end to end without a window:
//! provider → model listing → connection test → dependencies → interview
//! (questions, answers, plan, engine checks) → build (the real engine as a
//! subprocess) → results → edit (proposal, diff) → rebuild.
//!
//! `cargo run --example app_e2e -- <base_url> <model> [api_key] [projects_dir]`
//! Against the mock server in the development notes or any real provider.
use someda::app::runner::BuildEvent;
use someda::app::state::{AppState, Effect, Event, Screen};
use someda::app::{config::AppConfig, deps, interview::Phase};

fn run(state: &mut AppState, ev: Event) {
    let mut effect = state.apply(ev);
    loop {
        match effect {
            Effect::None => return,
            Effect::SaveConfig => return,
            Effect::ListModels { protocol, base_url, api_key } => {
                let gw = someda::gateway::Gateway::new(protocol, &base_url, &api_key, "");
                effect = state.apply(Event::ModelsListed(gw.list_models().map_err(|e| e.to_string())));
            }
            Effect::TestConnection { protocol, base_url, api_key, model } => {
                let gw = someda::gateway::Gateway::new(protocol, &base_url, &api_key, &model);
                effect = state.apply(Event::ConnectionTested(gw.chat("You are terse.", "Reply with the single word OK.").map_err(|e| e.to_string())));
            }
            Effect::Detect => effect = state.apply(Event::Detected(deps::detect(state.config.kicad_cli.as_deref(), state.config.someda_cli.as_deref()))),
            Effect::Install(_) => panic!("install not exercised here"),
            Effect::AskModel(call) => {
                let gw = someda::gateway::Gateway::new(state.protocol(), &state.config.base_url, &state.config.api_key, &state.config.model);
                effect = state.apply(Event::ModelReplied(gw.chat_with_temperature(&call.system, &call.user, Some(0.1)).map_err(|e| e.to_string())));
            }
            Effect::SpawnBuild(req) => {
                let rx = someda::app::runner::spawn(req);
                effect = Effect::None;
                for ev in rx {
                    if let BuildEvent::Line(l) = &ev {
                        if l.contains('[') && (l.contains('✓') || l.contains('✗')) || l.contains("repair:") || l.contains("HONEST") || l.contains("NOT DONE") {
                            println!("    {}", l.trim());
                        }
                    }
                    let e = state.apply(Event::BuildOutput(ev));
                    if e != Effect::None {
                        effect = e;
                    }
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base_url = args.get(1).cloned().unwrap_or_else(|| "http://127.0.0.1:8765/v1".into());
    let model = args.get(2).cloned().unwrap_or_else(|| "mock-planner".into());
    let api_key = args.get(3).cloned().unwrap_or_else(|| "none".into());
    let projects = args.get(4).cloned();
    let mut cfg = AppConfig { provider: "custom".into(), base_url: base_url.clone(), model: String::new(), api_key, projects_dir: projects, ..Default::default() };
    if base_url.contains("anthropic") {
        cfg.provider = "anthropic".into();
    }
    let mut st = AppState::with_config(cfg);
    println!("1. provider screen");
    run(&mut st, Event::Start);
    st.config.base_url = base_url;
    run(&mut st, Event::ListModels);
    println!("   models: {:?} → {}", st.models, st.status);
    if st.config.model.is_empty() {
        run(&mut st, Event::ModelEdited(model));
    }
    run(&mut st, Event::TestConnection);
    println!("   {}", st.status);
    run(&mut st, Event::ProviderDone);
    assert_eq!(st.screen, Screen::Dependencies, "provider step did not complete: {}", st.status);
    println!("2. dependencies");
    run(&mut st, Event::Detect);
    for c in &st.deps.as_ref().unwrap().checks {
        println!("   {} {}", if c.status.ok() { "✓" } else { "✗" }, c.status.text());
    }
    run(&mut st, Event::DependenciesDone);
    assert_eq!(st.screen, Screen::Interview, "{}", st.status);
    println!("3. interview");
    run(&mut st, Event::Describe("a 555 timer blinking an LED at about 1 Hz, 5 V in on a 2-pin header".into()));
    let mut rounds = 0;
    while st.interview.phase == Phase::Questions && rounds < 5 {
        rounds += 1;
        for (i, q) in st.interview.pending.iter().enumerate() {
            let pick = q.recommended.unwrap_or(0);
            println!("   Q: {} → {}", q.text, q.options.get(pick).cloned().unwrap_or_default());
            let _ = i;
        }
        run(&mut st, Event::SubmitAnswers);
    }
    for t in &st.interview.turns {
        if let someda::app::interview::Turn::Engine(e) = t {
            println!("   engine: {}", e);
        }
    }
    assert_eq!(st.interview.phase, Phase::Review, "interview did not reach review: {:?} {}", st.interview.error, st.status);
    let plan = st.interview.plan.clone().unwrap();
    println!("   plan: {} parts, {} nets; U1 = {}", plan.components.len(), plan.nets.len(), plan.components[0].lib_id);
    println!("4. build");
    run(&mut st, Event::StartBuild);
    assert_eq!(st.screen, Screen::Results, "{}", st.status);
    let s = st.summary.clone().unwrap();
    println!("   honest_done={} gates={:?} repairs={} exit={:?}", s.honest_done, s.gates, s.repairs.len(), st.build_exit);
    let out = someda::app::runner::outputs(&st.project.as_ref().unwrap().build_dir(st.version));
    println!("   render={:?} jlcpcb={:?}", out.iso_render.is_some(), out.jlcpcb_zip.is_some());
    println!("5. edit");
    run(&mut st, Event::EditRequested);
    run(&mut st, Event::EditPrompt("make R2 47k so it blinks slower".into()));
    println!("   proposal: {:?}", st.proposal_diff);
    assert!(st.proposal.is_some(), "no proposal: {}", st.status);
    run(&mut st, Event::ConfirmEdit);
    assert_eq!(st.screen, Screen::Results, "{}", st.status);
    let s2 = st.summary.clone().unwrap();
    println!("   v{} honest_done={} exit={:?}", st.version, s2.honest_done, st.build_exit);
    println!("E2E OK: {}", st.project.as_ref().unwrap().dir.display());
}
