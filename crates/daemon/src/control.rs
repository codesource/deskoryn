//! The Control-channel pump: heartbeat (liveness) plus handling of the
//! non-input control messages (`Ping`/`Pong`/`Goodbye`/`LayoutUpdate`).
//!
//! Runs for the lifetime of a session. It returns:
//! * `Ok(())` on a clean end (channel EOF or a peer `Goodbye`);
//! * `Err(..)` when the peer stops answering heartbeats (so the supervisor
//!   tears the session down and reconnects).
//!
//! The heartbeat is a simple unanswered-ping counter: each tick sends a `Ping`
//! and bumps `outstanding`; any received `Pong` resets it to zero. Reaching
//! `max_missed` declares the peer dead. This detects a half-open connection far
//! faster than QUIC's idle timeout.

use bytes::BytesMut;
use deskoryn_net::transport::{Sink, Source};
use deskoryn_proto::{decode_one, encode, Control};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug)]
pub struct HeartbeatConfig {
    pub interval: Duration,
    pub max_missed: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            max_missed: 5,
        }
    }
}

/// What ended the control pump (surfaced for logging/metrics).
#[derive(Debug, PartialEq, Eq)]
pub enum ControlEnd {
    /// Peer closed the channel.
    Eof,
    /// Peer sent `Goodbye`.
    Goodbye,
}

/// Drive the control channel until the peer goes away.
pub async fn run_control(
    sink: Box<dyn Sink>,
    source: Box<dyn Source>,
    hb: HeartbeatConfig,
) -> anyhow::Result<ControlEnd> {
    let sink = Arc::new(Mutex::new(sink));
    let outstanding = Arc::new(AtomicU32::new(0));

    // Reader: answer pings, reset liveness on pong, stop on goodbye/EOF.
    let mut reader = {
        let sink = sink.clone();
        let outstanding = outstanding.clone();
        tokio::spawn(async move { read_loop(source, sink, outstanding).await })
    };

    // Ticker: send pings, declare death after `max_missed` unanswered.
    let ticker = async {
        let mut iv = tokio::time::interval(hb.interval);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut nonce: u64 = 0;
        loop {
            iv.tick().await;
            if outstanding.load(Ordering::Relaxed) >= hb.max_missed {
                return Err(anyhow::anyhow!("heartbeat timeout: peer unresponsive"));
            }
            nonce += 1;
            let mut out = BytesMut::new();
            encode(&Control::Ping { nonce }, &mut out)?;
            if sink.lock().await.send_bytes(&out).await.is_err() {
                // Channel closed under us; let the reader report the real end.
                return Ok(());
            }
            outstanding.fetch_add(1, Ordering::Relaxed);
        }
    };

    let outcome = tokio::select! {
        r = &mut reader => r.unwrap_or_else(|e| Err(anyhow::anyhow!(e))),
        t = ticker => t.map(|_| ControlEnd::Eof),
    };
    reader.abort();
    outcome
}

async fn read_loop(
    mut source: Box<dyn Source>,
    sink: Arc<Mutex<Box<dyn Sink>>>,
    outstanding: Arc<AtomicU32>,
) -> anyhow::Result<ControlEnd> {
    loop {
        let frame = match source.recv_bytes().await {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(ControlEnd::Eof),
            Err(e) => return Err(anyhow::anyhow!(e)),
        };
        let mut buf = BytesMut::from(&frame[..]);
        match decode_one::<Control>(&mut buf) {
            Ok(Some(Control::Ping { nonce })) => {
                let mut out = BytesMut::new();
                if encode(&Control::Pong { nonce }, &mut out).is_ok() {
                    let _ = sink.lock().await.send_bytes(&out).await;
                }
            }
            Ok(Some(Control::Pong { .. })) => {
                outstanding.store(0, Ordering::Relaxed);
            }
            Ok(Some(Control::Goodbye { reason })) => {
                tracing::info!(%reason, "peer said goodbye");
                return Ok(ControlEnd::Goodbye);
            }
            Ok(Some(Control::LayoutUpdate { .. })) => {
                // TODO(impl): forward to the focus machine to rebuild the desktop.
                tracing::debug!("received layout update");
            }
            Ok(Some(other)) => tracing::debug!(?other, "control message"),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "control decode error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_core::DeviceId;
    use deskoryn_net::transport::{loopback, Session};
    use deskoryn_proto::Channel;

    fn fast_hb() -> HeartbeatConfig {
        HeartbeatConfig {
            interval: Duration::from_millis(20),
            max_missed: 3,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peers_keep_each_other_alive() {
        let (a, b) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let (ta, ra) = a.channel(Channel::Control).await.unwrap();
        let (tb, rb) = b.channel(Channel::Control).await.unwrap();

        let mut pa = tokio::spawn(run_control(ta, ra, fast_hb()));
        let mut pb = tokio::spawn(run_control(tb, rb, fast_hb()));

        // Over ~10 heartbeat intervals neither side should declare the other dead.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pa.is_finished(), "pump A ended unexpectedly");
        assert!(!pb.is_finished(), "pump B ended unexpectedly");

        pa.abort();
        pb.abort();
        let _ = (&mut pa, &mut pb);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresponsive_peer_trips_timeout() {
        let (a, b) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let (ta, ra) = a.channel(Channel::Control).await.unwrap();
        // Hold B's session (and its control channel) open but never answer.
        let _b_keepalive = b.channel(Channel::Control).await.unwrap();
        let _b = b;

        let result = tokio::time::timeout(Duration::from_secs(2), run_control(ta, ra, fast_hb())).await;
        let inner = result.expect("should resolve well before the 2s bound");
        assert!(inner.is_err(), "an unresponsive peer must trip the heartbeat timeout");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn goodbye_ends_cleanly() {
        let (a, b) = loopback::loopback(DeviceId::from_bytes([1; 16]), DeviceId::from_bytes([2; 16]));
        let (ta, ra) = a.channel(Channel::Control).await.unwrap();
        let (mut tb, _rb) = b.channel(Channel::Control).await.unwrap();

        let pump = tokio::spawn(run_control(ta, ra, fast_hb()));

        // Peer immediately says goodbye.
        let mut out = BytesMut::new();
        encode(&Control::Goodbye { reason: "shutting down".into() }, &mut out).unwrap();
        tb.send_bytes(&out).await.unwrap();

        let end = tokio::time::timeout(Duration::from_secs(2), pump)
            .await
            .expect("resolves")
            .expect("join")
            .expect("ok");
        assert_eq!(end, ControlEnd::Goodbye);
    }
}
