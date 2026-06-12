#![no_main]
//! Fuzz the wire decoders: arbitrary bytes must never make the decoder panic —
//! only return `Err`/`None`. Mirrors the `decoders_never_panic_on_garbage`
//! stable test with real coverage-guided input.

use bytes::BytesMut;
use deskoryn_proto::{decode_one, from_datagram, AudioFrame, Control, FileXfer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Stream framing (length-prefixed).
    let mut buf = BytesMut::from(data);
    let _ = decode_one::<Control>(&mut buf);
    let _ = decode_one::<FileXfer>(&mut buf);
    let _ = decode_one::<AudioFrame>(&mut buf);

    // Datagram decoding (unframed).
    let _ = from_datagram::<AudioFrame>(data);
    let _ = from_datagram::<Control>(data);
});
