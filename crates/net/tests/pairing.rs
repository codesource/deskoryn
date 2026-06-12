//! End-to-end pairing over real QUIC: two *unpaired* endpoints connect without a
//! trust check, exchange identities, and each derives the short authentication
//! string (SAS) from the certificate fingerprints + the shared TLS channel
//! binding. The test asserts both sides compute the **same** SAS and that
//! confirming the pairing produces trust records that authenticate a later
//! connection.
//!
//! Run with: `cargo test -p deskoryn-net --features quic`

#![cfg(feature = "quic")]

use std::sync::Arc;
use tokio::sync::Mutex;

use bytes::BytesMut;
use deskoryn_core::trust::TrustStore;
use deskoryn_core::DeviceId;
use deskoryn_net::pairing::PairingSession;
use deskoryn_net::quic::{DeviceIdentity, QuicEndpoint, QuicSession};
use deskoryn_net::transport::Session;
use deskoryn_proto::{decode_one, encode, Channel, Control, PROTOCOL_VERSION};

/// Exchange `Control::Hello` over the session's Control channel; return the
/// peer's (id, name).
async fn exchange(session: &QuicSession, local: DeviceId, name: &str) -> Result<(DeviceId, String), String> {
    let (mut sink, mut source) = session.channel(Channel::Control).await.map_err(|e| e.to_string())?;
    let hello = Control::Hello {
        version: PROTOCOL_VERSION,
        device: local,
        name: name.into(),
        monitors: Default::default(),
        capabilities: deskoryn_proto::Capabilities {
            clipboard_text: true,
            clipboard_images: true,
            clipboard_files: true,
            file_transfer: true,
            audio_forward: false,
        },
    };
    let mut buf = BytesMut::new();
    encode(&hello, &mut buf).map_err(|e| e.to_string())?;
    sink.send_bytes(&buf).await.map_err(|e| e.to_string())?;

    let frame = source
        .recv_bytes()
        .await
        .map_err(|e| format!("recv: {e}"))?
        .ok_or_else(|| "peer closed before hello".to_string())?;
    let mut b = BytesMut::from(&frame[..]);
    match decode_one::<Control>(&mut b).map_err(|e| e.to_string())?.ok_or("short frame")? {
        Control::Hello { device, name, .. } => Ok((device, name)),
        other => Err(format!("expected Hello, got {other:?}")),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_derives_matching_sas_and_pins_trust() {
    let id_a = DeviceId::generate();
    let id_b = DeviceId::generate();
    let identity_a = Arc::new(DeviceIdentity::generate(id_a).unwrap());
    let identity_b = Arc::new(DeviceIdentity::generate(id_b).unwrap());
    let fp_a = identity_a.fingerprint;
    let fp_b = identity_b.fingerprint;

    // Both start with EMPTY trust stores — they are not yet paired.
    let trust_a = Arc::new(Mutex::new(TrustStore::default()));
    let trust_b = Arc::new(Mutex::new(TrustStore::default()));

    let ep_b = QuicEndpoint::bind(0, identity_b.clone(), trust_b.clone()).await.unwrap();
    let port_b = ep_b.local_port();
    let ep_a = QuicEndpoint::bind(0, identity_a.clone(), trust_a.clone()).await.unwrap();
    let addr = format!("127.0.0.1:{port_b}").parse().unwrap();

    let client = async {
        let session = ep_a.connect_unverified(addr).await.map_err(|e| e.to_string())?;
        let (peer_id, peer_name) = exchange(&session, id_a, "device-a").await?;
        let cb = session.channel_binding().map_err(|e| e.to_string())?;
        let pairing = PairingSession::new(id_a, fp_a, peer_id, session.fingerprint(), peer_name, &cb);
        trust_a.lock().await.upsert(pairing.confirm(0, None));
        Ok::<_, String>((peer_id, pairing.sas))
    };

    let server = async {
        let session = ep_b.accept_unverified().await.map_err(|e| e.to_string())?;
        let (peer_id, peer_name) = exchange(&session, id_b, "device-b").await?;
        let cb = session.channel_binding().map_err(|e| e.to_string())?;
        let pairing = PairingSession::new(id_b, fp_b, peer_id, session.fingerprint(), peer_name, &cb);
        trust_b.lock().await.upsert(pairing.confirm(0, None));
        Ok::<_, String>((peer_id, pairing.sas))
    };

    let (client_res, server_res) = tokio::join!(client, server);
    let (a_saw, sas_a) = client_res.expect("client pairing failed");
    let (b_saw, sas_b) = server_res.expect("server pairing failed");

    assert_eq!(a_saw, id_b, "client identified the server");
    assert_eq!(b_saw, id_a, "server identified the client");

    // The crux: both screens show the same 6-digit code.
    assert_eq!(sas_a, sas_b, "SAS must match on both peers");

    // And the pins now authenticate each other for future connections.
    assert!(trust_a.lock().await.verify(id_b, &fp_b), "A pinned B");
    assert!(trust_b.lock().await.verify(id_a, &fp_a), "B pinned A");
}
