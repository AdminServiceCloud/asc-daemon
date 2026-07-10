//! Shared attach sessions: one output source per app, many WebSocket
//! subscribers (DMN-007).
//!
//! Modeled after the wings sink-pool design: the daemon keeps a single
//! attach to the app and fans its output out to every connected client
//! through a broadcast channel. A subscriber that falls behind loses the
//! oldest chunks but never blocks the source or other clients. A bounded
//! replay buffer gives a newly opened tab the recent output, and stdin
//! from all clients converges into the app's single input pipe. The
//! source lives exactly as long as the last client.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, Weak};

use anyhow::{Result, anyhow};
use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

/// Fan-out capacity per subscriber: falling further behind than this many
/// chunks drops the oldest ones for that subscriber only.
const BROADCAST_CAPACITY: usize = 256;
/// Replay buffer cap: how much recent output a new client receives.
const REPLAY_LIMIT_BYTES: usize = 128 * 1024;
/// Pending stdin writes across all clients of one session.
const STDIN_CAPACITY: usize = 64;

type Chunk = Vec<u8>;

/// Per-app shared sessions. The map holds weak refs and clients hold strong
/// ones, so a session disappears with its last client; a dead entry is
/// replaced on the next subscribe.
#[derive(Default)]
pub struct AttachHub {
    /// tokio mutex on purpose: held across the source connect await, so two
    /// tabs racing to open the first console cannot create two sources.
    sessions: Mutex<HashMap<String, Weak<AttachSession>>>,
}

impl AttachHub {
    /// Join the app's shared session; `connect` is awaited only when this is
    /// the first client (or the previous session's source already ended).
    pub async fn subscribe<S, W, F>(&self, app_id: &str, connect: F) -> Result<AttachClient>
    where
        F: Future<Output = Result<(S, W)>>,
        S: Stream<Item = Result<Chunk>> + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(app_id).and_then(Weak::upgrade)
            && let Some(client) = join(&session)
        {
            return Ok(client);
        }
        let (source, sink) = connect.await?;
        let session = Arc::new(spawn(app_id, source, sink));
        let client = join(&session).expect("fresh session is open");
        sessions.insert(app_id.to_string(), Arc::downgrade(&session));
        Ok(client)
    }
}

/// One subscriber: recent history plus the live receiver. Holding it keeps
/// the shared session — and therefore the source — alive.
pub struct AttachClient {
    pub session: Arc<AttachSession>,
    pub rx: broadcast::Receiver<Chunk>,
    /// Output that arrived before this client joined, oldest first.
    pub replay: Vec<Chunk>,
}

pub struct AttachSession {
    shared: Arc<Shared>,
    stdin: mpsc::Sender<Chunk>,
    pump: JoinHandle<()>,
}

impl AttachSession {
    /// Queue bytes for the app's stdin.
    pub async fn send_stdin(&self, data: Chunk) -> Result<()> {
        self.stdin
            .send(data)
            .await
            .map_err(|_| anyhow!("console source closed"))
    }
}

impl Drop for AttachSession {
    fn drop(&mut self) {
        // Last client gone: closing the pump drops the attach transport.
        self.pump.abort();
    }
}

/// State shared between the pump task and `join`.
struct Shared {
    /// `None` once the source ended; dropping the sender lets receivers
    /// drain what is buffered and then observe `Closed`.
    tx: StdMutex<Option<broadcast::Sender<Chunk>>>,
    replay: StdMutex<Replay>,
}

#[derive(Default)]
struct Replay {
    chunks: VecDeque<Chunk>,
    bytes: usize,
}

impl Replay {
    fn push(&mut self, chunk: &Chunk) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk.clone());
        while self.bytes > REPLAY_LIMIT_BYTES {
            let Some(dropped) = self.chunks.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
        }
    }
}

/// Subscribe to a live session; `None` when its source already ended.
///
/// Lock order (replay, then tx) must match the pump's publishing block:
/// that makes the history snapshot and the subscription one atomic point,
/// so every chunk is seen exactly once — in the replay or on the receiver.
fn join(session: &Arc<AttachSession>) -> Option<AttachClient> {
    let replay = session
        .shared
        .replay
        .lock()
        .expect("console replay lock poisoned");
    let tx = session.shared.tx.lock().expect("console tx lock poisoned");
    let rx = tx.as_ref()?.subscribe();
    Some(AttachClient {
        session: Arc::clone(session),
        rx,
        replay: replay.chunks.iter().cloned().collect(),
    })
}

fn spawn<S, W>(app_id: &str, mut source: S, mut sink: W) -> AttachSession
where
    S: Stream<Item = Result<Chunk>> + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let shared = Arc::new(Shared {
        tx: StdMutex::new(Some(broadcast::channel(BROADCAST_CAPACITY).0)),
        replay: StdMutex::default(),
    });
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Chunk>(STDIN_CAPACITY);

    let app = app_id.to_string();
    let pump_shared = Arc::clone(&shared);
    let publish = move |chunk: Chunk| {
        let mut replay = pump_shared
            .replay
            .lock()
            .expect("console replay lock poisoned");
        let tx = pump_shared.tx.lock().expect("console tx lock poisoned");
        replay.push(&chunk);
        if let Some(tx) = tx.as_ref() {
            // No receivers right now is fine: the replay keeps the output.
            let _ = tx.send(chunk);
        }
    };

    let end_shared = Arc::clone(&shared);
    let pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                chunk = source.next() => match chunk {
                    Some(Ok(chunk)) => publish(chunk),
                    Some(Err(err)) => {
                        publish(format!("error: {err:#}").into_bytes());
                        break;
                    }
                    None => break, // app stopped / source ended
                },
                data = stdin_rx.recv() => match data {
                    Some(data) => {
                        if let Err(err) = sink.write_all(&data).await {
                            debug!(app = %app, error = %err, "console stdin write failed");
                            break;
                        }
                    }
                    // All stdin senders live in the session; recv() only
                    // errors after it is dropped, and abort ends us first.
                    None => break,
                },
            }
        }
        // Drop the sender: clients drain buffered chunks, then see Closed.
        end_shared
            .tx
            .lock()
            .expect("console tx lock poisoned")
            .take();
    });

    AttachSession {
        shared,
        stdin: stdin_tx,
        pump,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, DuplexStream};

    type SourceStream = futures_util::stream::BoxStream<'static, Result<Chunk>>;

    /// Manually fed output source: send chunks through the returned sender.
    fn source() -> (mpsc::UnboundedSender<Result<Chunk>>, SourceStream) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
        (tx, stream.boxed())
    }

    fn sink() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(1024)
    }

    #[tokio::test]
    async fn fans_out_and_replays_to_late_subscribers() {
        let hub = AttachHub::default();
        let connects = AtomicUsize::new(0);

        let (out, stream) = source();
        let (write, _read) = sink();
        let mut first = hub
            .subscribe("demo", async {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok((stream, write))
            })
            .await
            .unwrap();
        assert!(first.replay.is_empty());
        out.send(Ok(b"boot".to_vec())).unwrap();
        assert_eq!(first.rx.recv().await.unwrap(), b"boot");

        // Second tab: session is reused, history is replayed.
        let (_out2, stream2) = source();
        let (write2, _read2) = sink();
        let mut second = hub
            .subscribe("demo", async {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok((stream2, write2))
            })
            .await
            .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 1, "source must be shared");
        assert_eq!(second.replay, vec![b"boot".to_vec()]);

        // Live output reaches both.
        out.send(Ok(b"tick".to_vec())).unwrap();
        assert_eq!(first.rx.recv().await.unwrap(), b"tick");
        assert_eq!(second.rx.recv().await.unwrap(), b"tick");
    }

    #[tokio::test]
    async fn stdin_from_any_client_reaches_the_sink() {
        let hub = AttachHub::default();
        let (_out, stream) = source();
        let (write, mut read) = sink();
        let first = hub
            .subscribe("demo", async { Ok((stream, write)) })
            .await
            .unwrap();

        let (_out2, stream2) = source();
        let (write2, _read2) = sink();
        let second = hub
            .subscribe("demo", async { Ok((stream2, write2)) })
            .await
            .unwrap();

        first
            .session
            .send_stdin(b"say hi\n".to_vec())
            .await
            .unwrap();
        second.session.send_stdin(b"stop\n".to_vec()).await.unwrap();
        let mut buf = [0u8; 12];
        read.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"say hi\nstop\n");
    }

    #[tokio::test]
    async fn source_end_closes_clients_and_next_subscribe_reconnects() {
        let hub = AttachHub::default();
        let (out, stream) = source();
        let (write, _read) = sink();
        let mut client = hub
            .subscribe("demo", async { Ok((stream, write)) })
            .await
            .unwrap();

        drop(out); // app stopped: source stream ends
        assert!(matches!(
            client.rx.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));

        // The dead session still has a live Arc (our client), but a new
        // subscriber must get a fresh source, not the closed one.
        let connects = AtomicUsize::new(0);
        let (_out2, stream2) = source();
        let (write2, _read2) = sink();
        let fresh = hub
            .subscribe("demo", async {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok((stream2, write2))
            })
            .await
            .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert!(fresh.replay.is_empty(), "new session starts clean");
    }

    #[tokio::test]
    async fn last_client_drop_stops_the_source() {
        let hub = AttachHub::default();
        let (_out, stream) = source();
        let (write, mut read) = sink();
        let client = hub
            .subscribe("demo", async { Ok((stream, write)) })
            .await
            .unwrap();

        drop(client); // last client: pump aborts, sink drops
        let mut buf = [0u8; 1];
        let n = read.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "sink must be closed once the last client leaves");
    }

    #[tokio::test]
    async fn replay_buffer_is_capped() {
        let hub = AttachHub::default();
        let (out, stream) = source();
        let (write, _read) = sink();
        let mut first = hub
            .subscribe("demo", async { Ok((stream, write)) })
            .await
            .unwrap();

        let old = vec![b'a'; REPLAY_LIMIT_BYTES];
        let recent = vec![b'b'; 16];
        out.send(Ok(old)).unwrap();
        out.send(Ok(recent.clone())).unwrap();
        first.rx.recv().await.unwrap();
        first.rx.recv().await.unwrap();

        let (_out2, stream2) = source();
        let (write2, _read2) = sink();
        let second = hub
            .subscribe("demo", async { Ok((stream2, write2)) })
            .await
            .unwrap();
        assert_eq!(second.replay, vec![recent], "oldest chunk must be trimmed");
    }
}
