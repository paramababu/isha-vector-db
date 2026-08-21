#![no_main]
use libfuzzer_sys::fuzz_target;
use vdb_format::wal;

fuzz_target!(|data: &[u8]| {
    let scan = wal::scan(data);
    // Whatever the input, the scan must report a consistent view: the bytes it declares valid
    // cannot exceed the input, and the committed set is always derivable without panicking.
    assert!(scan.valid_bytes <= data.len() as u64);
    let _ = scan.committed();

    // Every frame it accepted must re-encode to the same bytes.
    for frame in &scan.frames {
        let re = frame.encode().expect("a decoded frame must be encodable");
        let (again, _) = vdb_format::WalFrame::decode_at(&re, 0).unwrap();
        assert_eq!(&again, frame);
    }
});
