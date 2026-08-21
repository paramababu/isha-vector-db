#![no_main]
use libfuzzer_sys::fuzz_target;
use vdb_format::Catalog;

fuzz_target!(|data: &[u8]| {
    if let Ok(catalog) = Catalog::decode(data) {
        assert!(catalog.dimension > 0, "a decoded catalog must have a usable dimension");
        assert!(catalog.row_stride() > 0);
        let re = catalog.encode().expect("a decoded catalog must be encodable");
        assert_eq!(Catalog::decode(&re).unwrap(), catalog);
    }
});
