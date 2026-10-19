//! Run the desktop app's library checks on a plan file and time them:
//! `cargo run --release --example plan_check -- plan.json`
use std::time::Instant;
fn main() {
    someda::app::libfetch::register_env();
    let path = std::env::args().nth(1).unwrap_or_else(|| "examples/plan_555.json".into());
    let text = std::fs::read_to_string(&path).expect("plan file");
    let plan: someda::orchestrator::CircuitPlan = serde_json::from_str(&text).expect("valid plan");
    let t = Instant::now();
    let (plan, check) = someda::app::interview::check_plan(plan, &[]);
    let dt = t.elapsed();
    println!("{} parts checked in {:.0} ms: {} fix(es), {} issue(s), {} pin map(s)", plan.components.len(), dt.as_secs_f64() * 1000.0, check.fixes.len(), check.issues.len(), check.pin_maps.len());
    for f in &check.fixes {
        println!("  fixed: {}", f);
    }
    for i in &check.issues {
        println!("  issue: {}", i.describe());
    }
}
