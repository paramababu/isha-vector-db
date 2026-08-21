#![no_main]
use libfuzzer_sys::fuzz_target;
use vdb_format::{FileHeader, FileKind};

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = FileHeader::decode_any(data) {
        // A header that decoded must re-encode identically: the encoding is canonical.
        assert_eq!(FileHeader::decode_any(&header.encode()).unwrap(), header);
        let _ = header.check_file_len(data.len() as u64);
    }
    for kind in FileKind::ALL {
        let _ = FileHeader::decode(data, kind);
    }
});
