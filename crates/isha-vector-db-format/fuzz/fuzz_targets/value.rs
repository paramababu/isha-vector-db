#![no_main]
use libfuzzer_sys::fuzz_target;
use isha_vector_db_format::Value;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = Value::decode(data) {
        // Canonicality: the only byte sequence that decodes to this value is this one.
        let re = value.encode().expect("a decoded value must be encodable");
        assert_eq!(re, data, "decode/encode round-trip is not canonical");
        assert!(value.depth() <= isha_vector_db_format::MAX_VALUE_DEPTH);
    }
});
