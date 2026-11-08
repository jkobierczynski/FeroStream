//! What the agent reads from the windows of the program it runs, before
//! it goes to the client: size hints and icons (any X client can set
//! them to anything).

#![no_main]

#[allow(dead_code, clippy::all)]
#[path = "../../crates/agent/src/wm.rs"]
mod wm;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let words: Vec<u32> = data
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let (min_w, min_h, max_w, max_h) = wm::size_limits(&words);
    assert!(max_w == 0 || max_w >= min_w);
    assert!(max_h == 0 || max_h >= min_h);
    let side = data.first().map_or(32, |&b| u16::from(b));
    if let Some(image) = wm::pick_icon(&words, side) {
        let px = image.decode().expect("an icon the agent made decodes");
        assert_eq!(px.len(), image.pixel_bytes());
    }
});
