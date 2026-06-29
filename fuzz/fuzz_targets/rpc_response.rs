#![no_main]
//! Fuzz the RPC response decoder (the daemon→client frame). Same contract as the
//! request target: any byte string decodes to `Ok|Err`, never panics or
//! over-allocates. A compromised/buggy daemon — or a man-in-the-middle on the
//! socket path — must not be able to crash the TUI client via a malformed frame.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = arca_core::rpc::decode_response(data);
});
