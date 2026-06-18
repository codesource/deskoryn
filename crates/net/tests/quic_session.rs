//! End-to-end test of the real QUIC transport: two endpoints on localhost
//! mutually authenticate via certificate pinning, then exchange a framed
//! `Control::Hello` over the Control channel and an audio datagram.
//!
//! Run with: `cargo test -p deskoryn-net --features quic`

#![cfg(feature = "quic")]

use std::sync::Arc;
use tokio::sync::Mutex;

use bytes::BytesMut;
use deskoryn_core::trust::{TrustStore, TrustedDevice};
use deskoryn_core::DeviceId;
use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint};
use deskoryn_proto::{decode_one, encode, Channel, Control, PROTOCOL_VERSION};

fn trust_of(peer: DeviceId, id: &DeviceIdentity, name: &str) -> TrustStore {
    let mut store = TrustStore::default();
    store.upsert(TrustedDevice {
        id: peer,
        name: name.into(),
        fingerprint: id.fingerprint,
        paired_at: 0,
        last_address: None,
    });
    store
}

fn hello(device: DeviceId, name: &str) -> Control {
    Control::Hello {
        version: PROTOCOL_VERSION,
        device,
        name: name.into(),
        monitors: Default::default(),
        capabilities: deskoryn_proto::Capabilities {
            clipboard_text: true,
            clipboard_images: true,
            clipboard_files: true,
            file_transfer: true,
            audio_forward: true,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_auth_handshake_and_datagram() {
    let id_a = DeviceId::generate();
    let id_b = DeviceId::generate();
    let identity_a = Arc::new(DeviceIdentity::generate(id_a).unwrap());
    let identity_b = Arc::new(DeviceIdentity::generate(id_b).unwrap());

    // Each side pins the other's certificate fingerprint.
    let trust_a = Arc::new(Mutex::new(trust_of(id_b, &identity_b, "b")));
    let trust_b = Arc::new(Mutex::new(trust_of(id_a, &identity_a, "a")));

    let ep_b = QuicEndpoint::bind(0, identity_b, trust_b).await.unwrap();
    let port_b = ep_b.local_port();
    let ep_a = QuicEndpoint::bind(0, identity_a, trust_a).await.unwrap();

    // Server side: accept, read the client's Hello, send our own, recv a datagram.
    let server = tokio::spawn(async move {
        let session = ep_b.accept().await.expect("accept");
        assert_eq!(session.peer(), id_a, "server identifies client by pinned fp");

        let (mut sink, mut source) = session.channel(Channel::Control).await.unwrap();
        let frame = source.recv_bytes().await.unwrap().expect("client hello frame");
        let mut buf = BytesMut::from(&frame[..]);
        let msg: Control = decode_one(&mut buf).unwrap().unwrap();
        match msg {
            Control::Hello { device, name, .. } => {
                assert_eq!(device, id_a);
                assert_eq!(name, "device-a");
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        // Reply with our Hello.
        let mut out = BytesMut::new();
        encode(&hello(id_b, "device-b"), &mut out).unwrap();
        sink.send_bytes(&out).await.unwrap();

        // Receive one audio datagram.
        let dg = session.recv_datagram().await.unwrap().expect("datagram");
        assert_eq!(dg, b"opus-frame");
    });

    // Client side: connect (verifying the server's pin), send Hello, read reply.
    let addr = format!("127.0.0.1:{port_b}").parse().unwrap();
    let session = ep_a.connect(addr, id_b).await.expect("connect");
    assert_eq!(session.peer(), id_b);

    let (mut sink, mut source) = session.channel(Channel::Control).await.unwrap();
    let mut out = BytesMut::new();
    encode(&hello(id_a, "device-a"), &mut out).unwrap();
    sink.send_bytes(&out).await.unwrap();

    let frame = source.recv_bytes().await.unwrap().expect("server hello frame");
    let mut buf = BytesMut::from(&frame[..]);
    let msg: Control = decode_one(&mut buf).unwrap().unwrap();
    assert!(matches!(msg, Control::Hello { device, .. } if device == id_b));

    // Send an audio datagram the server is waiting for.
    session.send_datagram(b"opus-frame").await.unwrap();

    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_streams_open_and_accept() {
    let id_a = DeviceId::generate();
    let id_b = DeviceId::generate();
    let identity_a = Arc::new(DeviceIdentity::generate(id_a).unwrap());
    let identity_b = Arc::new(DeviceIdentity::generate(id_b).unwrap());
    let trust_a = Arc::new(Mutex::new(trust_of(id_b, &identity_b, "b")));
    let trust_b = Arc::new(Mutex::new(trust_of(id_a, &identity_a, "a")));

    let ep_b = QuicEndpoint::bind(0, identity_b, trust_b).await.unwrap();
    let port_b = ep_b.local_port();
    let ep_a = QuicEndpoint::bind(0, identity_a, trust_a).await.unwrap();

    // Server accepts a dedicated stream (beyond the fixed channels) and echoes.
    let server = tokio::spawn(async move {
        let session = ep_b.accept().await.expect("accept");
        let (mut sink, mut source) = session.accept_stream().await.unwrap().expect("stream");
        let frame = source.recv_bytes().await.unwrap().expect("frame");
        sink.send_bytes(&frame).await.unwrap(); // echo back
        // Hold the session open until the client has read the echo.
        let _ = source.recv_bytes().await;
    });

    let addr = format!("127.0.0.1:{port_b}").parse().unwrap();
    let session = ep_a.connect(addr, id_b).await.expect("connect");

    let (mut sink, mut source) = session.open_stream().await.unwrap();
    let mut payload = BytesMut::new();
    payload.extend_from_slice(&(5u32.to_be_bytes()));
    payload.extend_from_slice(b"hello");
    sink.send_bytes(&payload).await.unwrap();

    let echoed = source.recv_bytes().await.unwrap().expect("echo");
    assert_eq!(&echoed[..], &payload[..], "dedicated stream round-trips a frame");

    drop(session);
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_unpaired_peer() {
    let id_a = DeviceId::generate();
    let id_b = DeviceId::generate();
    let identity_a = Arc::new(DeviceIdentity::generate(id_a).unwrap());
    let identity_b = Arc::new(DeviceIdentity::generate(id_b).unwrap());

    // B trusts A, but A does NOT trust B (empty trust store) → A must refuse.
    let trust_a = Arc::new(Mutex::new(TrustStore::default()));
    let trust_b = Arc::new(Mutex::new(trust_of(id_a, &identity_a, "a")));

    let ep_b = QuicEndpoint::bind(0, identity_b, trust_b).await.unwrap();
    let port_b = ep_b.local_port();
    let ep_a = QuicEndpoint::bind(0, identity_a, trust_a).await.unwrap();

    // Keep an acceptor alive so the connection can actually form.
    let _server = tokio::spawn(async move {
        let _ = ep_b.accept().await;
    });

    let addr = format!("127.0.0.1:{port_b}").parse().unwrap();
    let result = ep_a.connect(addr, id_b).await;
    assert!(result.is_err(), "connecting to an unpinned peer must fail");
}
