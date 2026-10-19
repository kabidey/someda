//! Print the engine's physics outputs for fixed inputs so they can be
//! compared against independent reference formulas.
use someda::{formulas, si, stackup, thermal};
fn main() {
    // microstrip: 2-layer 1.6mm (h 1.53, er 4.5, t 0.035) and 6-layer 3313 (h 0.0994, er 4.1)
    for (name, er, h) in [("2L h1.53 er4.5", 4.5, 1.53), ("6L h0.0994 er4.1", 4.1, 0.0994), ("4L h0.2104 er4.4", 4.4, 0.2104)] {
        let w50 = formulas::microstrip_width_for_z0(50.0, er, h, 0.035);
        println!("microstrip_width_for_z0 {name}: w50={:.4} mm; z0(w)={:.2}", w50, formulas::microstrip_z0(er, h, w50, 0.035));
        println!("microstrip_z0 {name}: w0.127={:.2} w0.25={:.2} w0.5={:.2} w1.0={:.2}", formulas::microstrip_z0(er, h, 0.127, 0.035), formulas::microstrip_z0(er, h, 0.25, 0.035), formulas::microstrip_z0(er, h, 0.5, 0.035), formulas::microstrip_z0(er, h, 1.0, 0.035));
        println!("stackup::microstrip_z0 {name}: w0.25={:.2} w0.5={:.2}", stackup::microstrip_z0(0.25, h, 0.035, er), stackup::microstrip_z0(0.5, h, 0.035, er));
    }
    println!("stripline_z0_simple w0.2 b1.0 er4.4 = {:.2}; w0.35 = {:.2}", stackup::stripline_z0_simple(0.2, 1.0, 0.035, 4.4), stackup::stripline_z0_simple(0.35, 1.0, 0.035, 4.4));
    println!("ipc2152_current dT10 w10mil t1.4mil(1oz) = {:.3} A; dT10 w50mil = {:.3} A; dT20 w100mil = {:.3} A", formulas::ipc2152_current(10.0, 10.0, 1.4), formulas::ipc2152_current(10.0, 50.0, 1.4), formulas::ipc2152_current(20.0, 100.0, 1.4));
    println!("critical_length_mm rise1ns = {:.1}; rise100ps = {:.2}", formulas::critical_length_mm(1000.0), formulas::critical_length_mm(100.0));
    println!("wavelength_mm 1GHz er_eff3.2 = {:.1}; max_via_stub_mm 1GHz = {:.2}; 10GHz = {:.3}", formulas::wavelength_mm(1e9, 3.2), formulas::max_via_stub_mm(1e9, 3.2), formulas::max_via_stub_mm(10e9, 3.2));
    println!("crosstalk_spacing w0.127 = {:.3} (3W) / {:.3} (5W)", formulas::crosstalk_spacing(0.127, false), formulas::crosstalk_spacing(0.127, true));
    println!("via_pair_loop_nh h1.6 s1.0 d0.3 = {:.3}", formulas::via_pair_loop_nh(1.6, 1.0, 0.3));
    // Coupled microstrip (Kirschning-Jansen) and the fab pair tables
    let cm = formulas::coupled_microstrip(4.5, 0.2, 0.2, 0.2, 0.0);
    println!("coupled_microstrip er4.5 h0.2 w0.2 s0.2 t0: Zeven {:.1} Zodd {:.1} Zdiff {:.1} (PCBWay/IPC approx: Zdiff 111.4)", cm.z_even, cm.z_odd, cm.z_diff());
    for layers in [2u32, 4, 6] {
        let st = someda::materials::Stackup::for_layers(layers);
        for z in [90.0, 100.0] {
            match st.pair_geometry(z, 0.10, if layers <= 2 { 0.2 } else { 0.127 }) {
                Some(g) => println!("{}-layer {} Ω pair: w {:.4} gap {:.4} mm — {}", layers, z, g.width_mm, g.gap_mm, g.source),
                None => println!("{}-layer {} Ω pair: no cross-section", layers, z),
            }
        }
    }
    println!("junction_temp 25C 0.8W 45 = {:.1}; thermal_via_count 32.7mm2 = {}", formulas::junction_temp_c(25.0, 0.8, 45.0), formulas::thermal_via_count(32.7));
    println!("cap_srf 100nF esl 0.5nH = {:.1} MHz; 10uF esl 1nH = {:.2} MHz", formulas::cap_srf_hz(0.5e-9, 100e-9) / 1e6, formulas::cap_srf_hz(1e-9, 10e-6) / 1e6);
    println!("pdn_target 50mV 2A = {:.3} ohm", formulas::pdn_target_ohms(0.05, 2.0));
    println!("ground_bounce 1nH 1A 1ns = {:.3} V", formulas::ground_bounce_v(1.0, 1.0, 1.0));
    println!("trace_l_per_mm w0.127 h0.0994 = {:.4} nH/mm; w0.25 h1.53 = {:.4} nH/mm", si::trace_l_per_mm(0.127, 0.0994, 0.035), si::trace_l_per_mm(0.25, 1.53, 0.035));
    let z6 = formulas::microstrip_z0(4.1, 0.0994, 0.127, 0.035);
    println!("trace_c_per_mm w0.127 h0.0994 er4.1 z0 {:.1} = {:.4} pF/mm", z6, si::trace_c_per_mm(0.127, 0.0994, 4.1, z6));
    println!("swr 50->60 = {:.3}; 50->100 = {:.3}", si::swr(50.0, 60.0), si::swr(50.0, 100.0));
    println!("theta_ja LQFP-128 = {}; SOT-23 = {}; SOT-23-6 = {}; QFN = {}; SOIC-8 = {}", thermal::package_theta_ja("Package_QFP:LQFP-128_14x14mm_P0.4mm"), thermal::package_theta_ja("SOT-23"), thermal::package_theta_ja("SOT-23-6"), thermal::package_theta_ja("QFN-16"), thermal::package_theta_ja("SOIC-8"));
    println!("annular_ring 0.35/0.2 = {:.3}; via_aspect 1.6/0.2 = {:?}", formulas::annular_ring_mm(0.35, 0.2), formulas::via_aspect_ratio(1.6, 0.2));
    println!("length_match usb3 = {:?} ddr = {:?} pcie = {:?}", formulas::length_match_budget_mm("usb3"), formulas::length_match_budget_mm("ddr"), formulas::length_match_budget_mm("pcie"));
}
