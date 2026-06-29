#![no_main]
//! Fuzz the RPC request decoder — arca's primary untrusted boundary (the Unix
//! socket peer: u32-LE length prefix + JSON body). Arbitrary bytes must only ever
//! decode to `Ok|Err`, never panic or over-allocate; the `MAX_FRAME_BYTES` guard
//! is the OOM backstop, so the fuzzer also confirms an oversized length prefix is
//! rejected rather than driving a huge allocation.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = arca_core::rpc::decode_request(data);
});
