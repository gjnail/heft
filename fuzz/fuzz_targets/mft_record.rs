//! Feeds arbitrary bytes to the NTFS record parser. Run from the repository
//! root with a nightly toolchain:
//!
//!     cargo install cargo-fuzz
//!     cargo +nightly fuzz run mft_record
//!
//! The parser file has no dependencies outside `std`, so it's included
//! directly instead of depending on the (binary) heft crate.

#![no_main]

#[path = "../../src/scan/mft_parse.rs"]
#[allow(dead_code, unused_imports)]
mod mft_parse;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    mft_parse::fuzz_one(data);
});
