#![no_main]
use libfuzzer_sys::fuzz_target;
use isha_vector_db_format::Manifest;

fuzz_target!(|data: &[u8]| {
    if let Ok(manifest) = Manifest::decode(data) {
        let re = manifest.encode().expect("a decoded manifest must be encodable");
        assert_eq!(Manifest::decode(&re).unwrap(), manifest);
    }
    // Slot selection must survive two arbitrary buffers: this is what runs when a database is
    // opened after a crash, so it is the least acceptable place for a panic.
    let mid = data.len() / 2;
    let _ = Manifest::scan_slots(data.get(..mid), data.get(mid..));
});
