//! The [`Session`] abstraction: a secure, authenticated connection to one peer
//! that carries the logical channels defined in `deskoryn-proto`.
//!
//! Implementations map channels onto their transport (QUIC streams/datagrams).
//! Callers work only against these traits, so the daemon is identical whether it
//! runs over real QUIC or the in-memory loopback used in tests.

use async_trait::async_trait;
use deskoryn_proto::Channel;
use deskoryn_core::DeviceId;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session closed")]
    Closed,
    #[error("channel {0:?} not available")]
    NoChannel(Channel),
    #[error("framing error: {0}")]
    Framing(#[from] deskoryn_proto::FrameError),
    #[error("transport error: {0}")]
    Transport(String),
}

/// A typed, reliable, ordered byte channel carrying length-prefixed messages.
#[async_trait]
pub trait Sink: Send {
    /// Serialize and send one message (reliable channels only).
    async fn send_bytes(&mut self, frame: &[u8]) -> Result<(), SessionError>;
    async fn flush(&mut self) -> Result<(), SessionError>;
}

#[async_trait]
pub trait Source: Send {
    /// Receive the next length-delimited frame, or `None` at end of stream.
    async fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>, SessionError>;
}

/// One authenticated connection to a peer.
#[async_trait]
pub trait Session: Send + Sync {
    /// The verified identity of the remote peer (pinned during pairing).
    fn peer(&self) -> DeviceId;

    /// Open (or get) the bidirectional reliable stream backing `channel`.
    async fn channel(
        &self,
        channel: Channel,
    ) -> Result<(Box<dyn Sink>, Box<dyn Source>), SessionError>;

    /// Open a fresh, dedicated bidirectional stream independent of the
    /// per-channel streams — for a one-off transfer (a file transfer or a large
    /// clipboard payload). The caller writes a `proto::StreamPurpose` frame first
    /// so the peer's [`accept_stream`](Self::accept_stream) can route it. Multiple
    /// dedicated streams run concurrently without head-of-line-blocking each other
    /// or the channels, so several transfers proceed in parallel.
    async fn open_stream(&self) -> Result<(Box<dyn Sink>, Box<dyn Source>), SessionError>;

    /// Accept the next peer-initiated dedicated stream, or `None` when the
    /// session closes. The first frame on it is the sender's `StreamPurpose`.
    async fn accept_stream(
        &self,
    ) -> Result<Option<(Box<dyn Sink>, Box<dyn Source>)>, SessionError>;

    /// Send one unreliable datagram (used by the audio channel). Best-effort;
    /// returns `Err` only if datagrams are unsupported or the session is closed.
    async fn send_datagram(&self, bytes: &[u8]) -> Result<(), SessionError>;

    /// Receive the next datagram (audio frames), or `None` when closed.
    async fn recv_datagram(&self) -> Result<Option<Vec<u8>>, SessionError>;

    async fn close(&self, reason: &str);
}

/// In-memory loopback session pair, for tests and the daemon's `--dry-run`.
///
/// `loopback()` returns two [`Session`] halves wired to each other so the full
/// daemon can run end-to-end in one process without any sockets.
pub mod loopback {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    type Bytes = Vec<u8>;
    type SharedRx = Arc<Mutex<mpsc::Receiver<Bytes>>>;
    /// Per-channel queue: our own sender + the shared receiver we read from.
    type ChannelMap = HashMap<u8, (mpsc::Sender<Bytes>, SharedRx)>;

    /// A dedicated bidirectional stream as a pair of byte sink/source.
    type DynStream = (Box<dyn Sink>, Box<dyn Source>);

    pub struct LoopSession {
        peer: DeviceId,
        // One mpsc pair per channel, created lazily.
        inner: Arc<Mutex<ChannelMap>>,
        partner_tx: Arc<Mutex<HashMap<u8, mpsc::Sender<Bytes>>>>,
        dgram_tx: mpsc::Sender<Bytes>,
        dgram_rx: Arc<Mutex<mpsc::Receiver<Bytes>>>,
        // Dedicated streams: `open_stream` builds a fresh pipe pair and hands the
        // peer's end to the partner's accept queue.
        dyn_to_partner: mpsc::Sender<DynStream>,
        dyn_accept: Arc<Mutex<mpsc::Receiver<DynStream>>>,
    }

    fn key(c: Channel) -> u8 {
        match c {
            Channel::Control => 0,
            Channel::Input => 1,
            Channel::Clipboard => 2,
            Channel::FileXfer => 3,
            Channel::Audio => 4,
        }
    }

    struct LoopSink(mpsc::Sender<Bytes>);
    struct LoopSource(Arc<Mutex<mpsc::Receiver<Bytes>>>);

    #[async_trait]
    impl Sink for LoopSink {
        async fn send_bytes(&mut self, frame: &[u8]) -> Result<(), SessionError> {
            self.0.send(frame.to_vec()).await.map_err(|_| SessionError::Closed)
        }
        async fn flush(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[async_trait]
    impl Source for LoopSource {
        async fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
            Ok(self.0.lock().await.recv().await)
        }
    }

    #[async_trait]
    impl Session for LoopSession {
        fn peer(&self) -> DeviceId {
            self.peer
        }

        async fn channel(
            &self,
            channel: Channel,
        ) -> Result<(Box<dyn Sink>, Box<dyn Source>), SessionError> {
            let k = key(channel);
            // Sender side goes to the partner; receiver side is our own queue.
            let tx = self
                .partner_tx
                .lock()
                .await
                .get(&k)
                .cloned()
                .ok_or(SessionError::NoChannel(channel))?;
            let rx = self
                .inner
                .lock()
                .await
                .get(&k)
                .map(|(_, r)| r.clone())
                .ok_or(SessionError::NoChannel(channel))?;
            Ok((Box::new(LoopSink(tx)), Box::new(LoopSource(rx))))
        }

        async fn open_stream(&self) -> Result<(Box<dyn Sink>, Box<dyn Source>), SessionError> {
            // Two independent directions, like a real bidirectional stream.
            let (ours_tx, ours_rx) = mpsc::channel(1024); // our writes -> partner reads
            let (theirs_tx, theirs_rx) = mpsc::channel(1024); // partner writes -> our reads
            let our_end: DynStream = (
                Box::new(LoopSink(ours_tx)),
                Box::new(LoopSource(Arc::new(Mutex::new(theirs_rx)))),
            );
            let partner_end: DynStream = (
                Box::new(LoopSink(theirs_tx)),
                Box::new(LoopSource(Arc::new(Mutex::new(ours_rx)))),
            );
            self.dyn_to_partner
                .send(partner_end)
                .await
                .map_err(|_| SessionError::Closed)?;
            Ok(our_end)
        }

        async fn accept_stream(
            &self,
        ) -> Result<Option<(Box<dyn Sink>, Box<dyn Source>)>, SessionError> {
            Ok(self.dyn_accept.lock().await.recv().await)
        }

        async fn send_datagram(&self, bytes: &[u8]) -> Result<(), SessionError> {
            self.dgram_tx.send(bytes.to_vec()).await.map_err(|_| SessionError::Closed)
        }

        async fn recv_datagram(&self) -> Result<Option<Vec<u8>>, SessionError> {
            Ok(self.dgram_rx.lock().await.recv().await)
        }

        async fn close(&self, _reason: &str) {}
    }

    /// Build two cross-wired sessions for `a` and `b`.
    pub fn loopback(a: DeviceId, b: DeviceId) -> (LoopSession, LoopSession) {
        let mut a_inner = HashMap::new();
        let mut b_inner = HashMap::new();
        let mut a_partner = HashMap::new();
        let mut b_partner = HashMap::new();

        for k in 0u8..5 {
            let (a_tx, a_rx) = mpsc::channel(1024);
            let (b_tx, b_rx) = mpsc::channel(1024);
            a_inner.insert(k, (a_tx.clone(), Arc::new(Mutex::new(a_rx))));
            b_inner.insert(k, (b_tx.clone(), Arc::new(Mutex::new(b_rx))));
            // a's sink delivers into b's queue, and vice versa.
            a_partner.insert(k, b_tx);
            b_partner.insert(k, a_tx);
        }

        let (a_dtx, a_drx) = mpsc::channel(1024);
        let (b_dtx, b_drx) = mpsc::channel(1024);

        // Dedicated-stream accept queues: A opening a stream enqueues onto B's
        // accept queue, and vice versa.
        let (a_dyn_tx, a_dyn_rx) = mpsc::channel(1024);
        let (b_dyn_tx, b_dyn_rx) = mpsc::channel(1024);

        let a_sess = LoopSession {
            peer: b,
            inner: Arc::new(Mutex::new(a_inner)),
            partner_tx: Arc::new(Mutex::new(a_partner)),
            dgram_tx: b_dtx,
            dgram_rx: Arc::new(Mutex::new(a_drx)),
            dyn_to_partner: b_dyn_tx,
            dyn_accept: Arc::new(Mutex::new(a_dyn_rx)),
        };
        let b_sess = LoopSession {
            peer: a,
            inner: Arc::new(Mutex::new(b_inner)),
            partner_tx: Arc::new(Mutex::new(b_partner)),
            dgram_tx: a_dtx,
            dgram_rx: Arc::new(Mutex::new(b_drx)),
            dyn_to_partner: a_dyn_tx,
            dyn_accept: Arc::new(Mutex::new(b_dyn_rx)),
        };
        (a_sess, b_sess)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_core::DeviceId;

    #[tokio::test]
    async fn loopback_round_trips_a_frame() {
        let a = DeviceId::from_bytes([1; 16]);
        let b = DeviceId::from_bytes([2; 16]);
        let (sa, sb) = loopback::loopback(a, b);
        assert_eq!(sa.peer(), b);

        let (mut tx, _src) = sa.channel(Channel::Control).await.unwrap();
        let (_sink, mut rx) = sb.channel(Channel::Control).await.unwrap();
        tx.send_bytes(b"hello").await.unwrap();
        let got = rx.recv_bytes().await.unwrap().unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedicated_streams_are_independent_and_bidirectional() {
        let a = DeviceId::from_bytes([1; 16]);
        let b = DeviceId::from_bytes([2; 16]);
        let (sa, sb) = loopback::loopback(a, b);

        // Two concurrent dedicated streams; each carries its own data.
        let (mut a1_tx, _a1) = sa.open_stream().await.unwrap();
        let (mut a2_tx, mut a2_rx) = sa.open_stream().await.unwrap();
        a1_tx.send_bytes(b"stream-one").await.unwrap();
        a2_tx.send_bytes(b"stream-two").await.unwrap();

        // B accepts them in open order and reads each independently.
        let (_b1_tx, mut b1_rx) = sb.accept_stream().await.unwrap().unwrap();
        let (mut b2_tx, mut b2_rx) = sb.accept_stream().await.unwrap().unwrap();
        assert_eq!(b1_rx.recv_bytes().await.unwrap().unwrap(), b"stream-one");
        assert_eq!(b2_rx.recv_bytes().await.unwrap().unwrap(), b"stream-two");

        // Reverse direction works on the same stream.
        b2_tx.send_bytes(b"reply-two").await.unwrap();
        assert_eq!(a2_rx.recv_bytes().await.unwrap().unwrap(), b"reply-two");
    }
}
