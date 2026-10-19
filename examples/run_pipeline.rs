// E2E: plan.json → place → route → DRC → preflight (the idea-to-PCB spine)
fn main() {
    let plan_json = std::fs::read_to_string("plan.json").expect("plan.json");
    let plan: someda::orchestrator::CircuitPlan = serde_json::from_str(&plan_json).expect("valid plan");
    someda::orchestrator::validate_plan(&plan).expect("plan validates");

    let mut board = someda::orchestrator::generate_board_model(&plan);

    let nl = someda::netlist::Netlist::from_board(&board);
    let (w, h) = board.outline_bbox().map(|(x0,y0,x1,y1)| (x1-x0, y1-y0)).unwrap();
    let placer = someda::placer::Placer::new(w, h);
    let solved = placer.arrange(&board, &nl, &someda::placer::PlacementIntent::default());
    for s in &solved {
        if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
            let (dx, dy) = (s.x - fp.x, s.y - fp.y);
            fp.x = s.x; fp.y = s.y;
            for pad in &mut fp.pads { pad.x += dx; pad.y += dy; }
        }
    }
    println!("placement: {} footprints", solved.len());

    let nl = someda::netlist::Netlist::from_board(&board);
    let router = someda::router::Router::new(someda::router::RouterConfig::default());
    let route = router.route(&board, &nl);
    println!("route: {}", route.summary_line());
    for u in &route.unrouted { println!("  UNROUTED net {}: {}", u.net.1, u.cause); }
    for seg in &route.segments {
        board.segments.push(someda::board::Segment {
            start: (seg.x1, seg.y1), end: (seg.x2, seg.y2),
            width: seg.width, layer: seg.layer.clone(), net: Some(seg.net.clone()),
        });
    }
    for via in &route.vias {
        board.vias.push(someda::board::Via {
            x: via.x, y: via.y, size: via.size, drill: via.drill,
            layers: vec!["F.Cu".into(), "B.Cu".into()], net: Some(via.net.clone()),
        });
    }

    std::fs::write("circuit.kicad_pcb", board.to_kicad_pcb()).expect("write pcb");

    let report = someda::drc::run_drc(&board, &someda::drc::DrcConfig::default());
    println!("someda DRC: {} errors, {} warnings", report.errors(), report.warnings());
    for f in &report.findings { println!("  {}: {}", f.code, f.message); }

    let gates = someda::formulas::export_preflight(
        report.errors(), 0, route.unrouted.len(), true, true, true);
    for g in &gates { println!("preflight {}: {} ({})", g.name, g.passed, g.detail); }
}
