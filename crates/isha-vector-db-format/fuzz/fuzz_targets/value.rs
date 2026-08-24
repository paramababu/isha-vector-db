#![no_main]
use libfuzzer_sys::fuzz_target;
use isha_vector_db_format::Value;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = Value::decode(data) else {
        return;
    };
    let re = value.encode().expect("a decoded value must be encodable");

    // Canonicality, in the form the format actually promises (ADR-0014): the encoder has one
    // output per value, and that output is a fixed point — decoding it yields the same value,
    // and encoding again yields the same bytes.
    assert_eq!(
        Value::decode(&re).as_ref().ok(),
        Some(&value),
        "re-encoding changed the value"
    );
    assert_eq!(value.encode().unwrap(), re, "encoding is not deterministic");

    // Going the other way, `data` must be that same output — with one documented exception. A
    // v1 file writes a map of eight or more fields under the plain tag, a v2 build must still
    // read it, and re-encoding upgrades it to the indexed form. That upgrade only ever adds
    // the offset table, so the bytes get strictly longer; anything else that fails to
    // round-trip is the decoder accepting an encoding its own encoder would never produce.
    if re != data {
        assert!(
            re.len() > data.len(),
            "decode/encode round-trip is not canonical: {data:?} decoded to {value:?} and \
             re-encoded as {re:?}"
        );
    }

    assert!(value.depth() <= isha_vector_db_format::MAX_VALUE_DEPTH);
});
