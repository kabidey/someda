fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "examples/plan_555.json".to_string());
    let plan_json = std::fs::read_to_string(&arg).expect("plan");
    let plan: someda::orchestrator::CircuitPlan = serde_json::from_str(&plan_json).expect("valid plan");
    someda::orchestrator::validate_plan(&plan).expect("plan validates");

    // Pin coverage FIRST (the honest gate)
    let mut board = someda::orchestrator::generate_board_model(&plan);
    someda::orchestrator::verify_pin_coverage(&plan, &board).expect("pin coverage");

    // Place
    let nl = someda::netlist::Netlist::from_board(&board);
    let (w, h) = board.outline_bbox().map(|(x0,y0,x1,y1)| (x1-x0, y1-y0)).unwrap();
    let placer = someda::placer::Placer::new(w, h);
    let solved = placer.arrange_courtyards(&board, &nl, &someda::placer::PlacementIntent::default());
    for s in &solved {
        if let Some(fp) = board.footprints.iter_mut().find(|f| f.reference == s.reference) {
            let (dx, dy) = (s.x - fp.x, s.y - fp.y);
            fp.x = s.x; fp.y = s.y;
            for pad in &mut fp.pads { pad.x += dx; pad.y += dy; }
        }
    }
    println!("placement: {} footprints", solved.len());

    // Route
    let nl = someda::netlist::Netlist::from_board(&board);
    let router = someda::router::Router::new(someda::router::RouterConfig::default());
    let route = router.route(&board, &nl);
    println!("route: {}", route.summary_line());
    for u in &route.unrouted { println!("  UNROUTED {}: {}", u.net.1, u.cause); }
    // Find which ratsnest edge(s) failed: match unrouted nets back to edges
    for e in &nl.edges {
        if route.unrouted.iter().any(|u| u.net.1 == e.net) {
            println!("    candidate edge: {} {}:{}({:.1},{:.1}) -> {}:{:.1}({:.1},{:.1})",
                e.net, e.from_ref, e.from_pin, e.from_x, e.from_y, e.to_ref, e.to_pin, e.to_x, e.to_y);
        }
    }
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
    std::fs::write("circuit.kicad_sch", someda::orchestrator::generate_schematic(&plan)).expect("write sch");

    let report = someda::drc::run_drc(&board, &someda::drc::DrcConfig::default());
    println!("someda DRC: {} errors, {} warnings", report.errors(), report.warnings());

    let gates = someda::formulas::export_preflight(
        report.errors(), 0, route.unrouted.len(), true, true, true);
    let all = gates.iter().all(|g| g.passed);
    println!("preflight: {}", if all { "ALL PASS" } else { "FAIL" });
}
