#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The parser must never panic or OOM on any input.
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(parsed) = someda::sexp::parse(s) {
            let _ = parsed.to_string();
        }
    }
});
