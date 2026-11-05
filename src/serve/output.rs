//! Response output off the main loop (NWS-2962).
//!
//! The loop never writes to a client. Each socket connection has its own
//! writer thread fed by a bounded queue ([`MAX_QUEUED_FRAMES`] frames,
//! [`MAX_QUEUED_BYTES`] bytes); each response gets one overall
//! [`RESPONSE_DEADLINE`] from its first byte, re-armed per write call only
//! with the time *left*, so a client that trickle-reads cannot renew it.
//! A full queue or a missed deadline drops the connection: its socket is
//! shut down, its queue discarded, and its cancel flag set so read workers
//! skip its pending reads. Stdio has a single client (the agent), so its
//! writer thread has an unbounded queue and no drop policy: dropping the
//! only client would be fatal, and a broken stdout still shuts serve down.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Frames a socket connection may have queued or in flight before it is
/// dropped as not reading.
pub(crate) const MAX_QUEUED_FRAMES: usize = 32;

/// Bound the reader/writer threads retained by socket clients.
pub(crate) const MAX_CONNECTIONS: usize = 64;

/// Bytes a socket connection may have queued or in flight before it is
/// dropped. A single frame larger than this is still accepted onto an
/// empty queue, so no response is undeliverable by size alone.
pub(crate) const MAX_QUEUED_BYTES: usize = 64 << 20;

/// Overall time to write one response, from its first byte. Local socket
/// clients read a 20 MB response in well under a second; 20 s leaves room
/// for a slow-but-reading client and still frees the writer from one that
/// stopped.
pub(crate) const RESPONSE_DEADLINE: Duration = Duration::from_secs(20);

/// Bytes per write call: the remaining deadline is re-applied before each.
const WRITE_CHUNK: usize = 64 << 10;

/// Socket connections' output halves, by connection id — the loop's only
/// way to route a response to the connection that asked. A connection's
/// entry is removed when its reader thread ends (EOF or read error); a
/// response for a removed connection is dropped.
#[derive(Default)]
pub(crate) struct Connections {
    state: Mutex<ConnsState>,
    writers: Arc<Live>,
}

#[derive(Default)]
struct ConnsState {
    conns: BTreeMap<u64, Arc<Conn>>,
    /// Readers that ended while their writers still flush queued frames.
    retiring: Vec<Arc<Conn>>,
    /// Shutdown started: refuse new connections.
    closed: bool,
}

/// Live writer threads, so shutdown can wait for queued frames to flush.
#[derive(Default)]
struct Live {
    count: Mutex<usize>,
    idle: Condvar,
}

pub(crate) type ConnRegistry = Arc<Connections>;

struct Conn {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// Set when the connection closes or is dropped: its pending reads are
    /// not worth computing.
    cancelled: Arc<AtomicBool>,
    /// The client sent `initialize`, so it gets notifications such as
    /// `notifications/tools/list_changed` (NWS-2959). The CLI never does.
    notify: AtomicBool,
    writer_done: AtomicBool,
    stream: UnixStream,
}

#[derive(Default)]
struct Queue {
    frames: VecDeque<String>,
    /// Bytes queued plus the frame being written.
    bytes: usize,
    /// The writer removed one frame from `frames` but has not finished it.
    in_flight: bool,
    /// No new frames; the writer exits once the queue is empty.
    closing: bool,
}

impl Conn {
    fn enqueue(&self, frame: String) {
        let mut queue = self.queue.lock().unwrap();
        if queue.closing {
            return;
        }
        let busy = queue.bytes > 0 || !queue.frames.is_empty();
        if busy
            && (queue.frames.len() + usize::from(queue.in_flight) >= MAX_QUEUED_FRAMES
                || queue.bytes + frame.len() > MAX_QUEUED_BYTES)
        {
            drop(queue);
            self.abort();
            return;
        }
        queue.bytes += frame.len();
        queue.frames.push_back(frame);
        self.wake.notify_one();
    }

    /// Clean close: accept no new frames, flush what is queued.
    fn close(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.queue.lock().unwrap().closing = true;
        self.wake.notify_one();
    }

    /// Drop the connection now: discard its queue and shut the socket, which
    /// also ends its reader thread.
    fn abort(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        {
            let mut queue = self.queue.lock().unwrap();
            queue.closing = true;
            queue.frames.clear();
        }
        self.wake.notify_one();
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    fn next_frame(&self) -> Option<String> {
        let mut queue = self.queue.lock().unwrap();
        loop {
            if let Some(frame) = queue.frames.pop_front() {
                queue.in_flight = true;
                return Some(frame);
            }
            if queue.closing {
                return None;
            }
            queue = self.wake.wait(queue).unwrap();
        }
    }

    fn run_writer(&self) {
        while let Some(frame) = self.next_frame() {
            let written = write_within(&self.stream, frame.as_bytes(), RESPONSE_DEADLINE);
            {
                let mut queue = self.queue.lock().unwrap();
                queue.bytes -= frame.len();
                queue.in_flight = false;
            }
            if !written {
                self.abort();
                return;
            }
        }
    }
}

/// Write all of `bytes` within one overall `deadline`.
fn write_within(stream: &UnixStream, bytes: &[u8], deadline: Duration) -> bool {
    let deadline = Instant::now() + deadline;
    let mut written = 0;
    while written < bytes.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_write_timeout(Some(left)).is_err() {
            return false;
        }
        let end = (written + WRITE_CHUNK).min(bytes.len());
        match (&*stream).write(&bytes[written..end]) {
            Ok(0) => return false,
            Ok(n) => written += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

impl Connections {
    /// Register `conn_id`'s write half and start its writer thread. Called
    /// before the connection's reader starts, so every response has
    /// somewhere to go.
    pub(crate) fn register(&self, conn_id: u64, stream: UnixStream) -> std::io::Result<()> {
        let conn = Arc::new(Conn {
            queue: Mutex::default(),
            wake: Condvar::new(),
            cancelled: Arc::default(),
            notify: AtomicBool::new(false),
            writer_done: AtomicBool::new(false),
            stream,
        });
        {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(std::io::Error::other("figmog serve is shutting down"));
            }
            state
                .retiring
                .retain(|conn| !conn.writer_done.load(Ordering::Acquire));
            if state.conns.len() + state.retiring.len() >= MAX_CONNECTIONS {
                return Err(std::io::Error::other("too many socket connections"));
            }
            state.conns.insert(conn_id, conn.clone());
        }
        *self.writers.count.lock().unwrap() += 1;
        let live = self.writers.clone();
        let worker_conn = conn.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("figmog-conn-{conn_id}"))
            .spawn(move || {
                worker_conn.run_writer();
                worker_conn.writer_done.store(true, Ordering::Release);
                drop(worker_conn);
                let mut count = live.count.lock().unwrap();
                *count -= 1;
                live.idle.notify_all();
            });
        if let Err(error) = spawned {
            *self.writers.count.lock().unwrap() -= 1;
            conn.writer_done.store(true, Ordering::Release);
            self.unregister(conn_id);
            return Err(error);
        }
        Ok(())
    }

    /// The connection's reader ended: no new responses for it, pending
    /// reads cancelled, already-queued frames still flushed.
    pub(crate) fn unregister(&self, conn_id: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(conn) = state.conns.remove(&conn_id) {
            conn.close();
            state.retiring.push(conn);
        }
        state
            .retiring
            .retain(|conn| !conn.writer_done.load(Ordering::Acquire));
    }

    /// Queue one newline-terminated response frame for `conn_id`. A
    /// connection that is gone drops it; one whose queue is full is
    /// dropped itself.
    pub(crate) fn send(&self, conn_id: u64, frame: String) {
        let conn = self.state.lock().unwrap().conns.get(&conn_id).cloned();
        if let Some(conn) = conn {
            conn.enqueue(frame);
        }
    }

    /// `conn_id` sent `initialize`: send it notifications from now on.
    pub(crate) fn subscribe(&self, conn_id: u64) {
        if let Some(conn) = self.state.lock().unwrap().conns.get(&conn_id) {
            conn.notify.store(true, Ordering::Relaxed);
        }
    }

    /// Queue a notification frame for every subscribed connection, in
    /// connection-id order. Each takes its place in that connection's
    /// queue, behind responses already queued and ahead of later ones.
    pub(crate) fn broadcast(&self, frame: &str) {
        let subscribed: Vec<Arc<Conn>> = {
            let state = self.state.lock().unwrap();
            state
                .conns
                .values()
                .filter(|conn| conn.notify.load(Ordering::Relaxed))
                .cloned()
                .collect()
        };
        for conn in subscribed {
            conn.enqueue(frame.to_string());
        }
    }

    /// `conn_id`'s cancel flag: already set for a connection that is gone.
    pub(crate) fn cancel_flag(&self, conn_id: u64) -> Arc<AtomicBool> {
        match self.state.lock().unwrap().conns.get(&conn_id) {
            Some(conn) => conn.cancelled.clone(),
            None => Arc::new(AtomicBool::new(true)),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_registered(&self, conn_id: u64) -> bool {
        self.state.lock().unwrap().conns.contains_key(&conn_id)
    }

    /// Shutdown: refuse new connections, close every open one, and wait
    /// until `deadline` for their writers to flush queued frames.
    pub(crate) fn finish(&self, deadline: Instant) {
        let conns = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            let mut conns: Vec<_> = std::mem::take(&mut state.conns).into_values().collect();
            conns.append(&mut state.retiring);
            conns
        };
        for conn in &conns {
            conn.close();
        }
        let mut count = self.writers.count.lock().unwrap();
        while *count > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(count);
                for conn in &conns {
                    conn.abort();
                }
                return;
            }
            count = self.writers.idle.wait_timeout(count, left).unwrap().0;
        }
    }
}

impl Drop for Connections {
    /// Never blocks: every still-registered connection is closed, so its
    /// writer flushes what is queued (within each response's deadline),
    /// exits and releases the socket, instead of waiting for frames that
    /// can no longer arrive.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for conn in std::mem::take(&mut state.conns)
            .into_values()
            .chain(state.retiring.drain(..))
        {
            conn.close();
        }
    }
}

/// The stdio writer thread. Frames are written in the order queued.
pub(crate) struct StdioWriter {
    frames: Option<mpsc::Sender<String>>,
    gone: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StdioWriter {
    /// `write` gets each frame without its trailing newline and returns
    /// `false` once stdout is gone.
    pub(crate) fn spawn(mut write: impl FnMut(&str) -> bool + Send + 'static) -> Self {
        let (frames, queue) = mpsc::channel::<String>();
        let gone = Arc::new(AtomicBool::new(false));
        let flag = gone.clone();
        let thread = std::thread::Builder::new()
            .name("figmog-stdout".into())
            .spawn(move || {
                for frame in queue {
                    if !write(frame.strip_suffix('\n').unwrap_or(&frame)) {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            })
            .expect("spawn stdout writer");
        Self {
            frames: Some(frames),
            gone,
            thread: Some(thread),
        }
    }

    /// Queue one newline-terminated frame; `false` once stdout is gone.
    pub(crate) fn send(&self, frame: String) -> bool {
        !self.gone()
            && self
                .frames
                .as_ref()
                .is_some_and(|frames| frames.send(frame).is_ok())
    }

    pub(crate) fn gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// Flush queued frames, waiting until `deadline`. A panic in the
    /// writer (a test's output adapter) is re-raised here.
    pub(crate) fn finish(mut self, deadline: Instant) {
        self.frames.take();
        let Some(thread) = self.thread.take() else {
            return;
        };
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Err(panic) = thread.join() {
            std::panic::resume_unwind(panic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn bare_conn(stream: UnixStream) -> Conn {
        Conn {
            queue: Mutex::default(),
            wake: Condvar::new(),
            cancelled: Arc::default(),
            notify: AtomicBool::new(false),
            writer_done: AtomicBool::new(false),
            stream,
        }
    }

    #[test]
    fn the_frame_limit_counts_the_frame_already_taken_by_the_writer() {
        let (server, _client) = UnixStream::pair().unwrap();
        let conn = bare_conn(server);
        conn.enqueue("in flight\n".into());
        assert_eq!(conn.next_frame().as_deref(), Some("in flight\n"));
        for _ in 1..MAX_QUEUED_FRAMES {
            conn.enqueue("queued\n".into());
        }
        assert!(!conn.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            conn.queue.lock().unwrap().frames.len(),
            MAX_QUEUED_FRAMES - 1
        );
        conn.enqueue("over limit\n".into());
        assert!(conn.cancelled.load(Ordering::Relaxed));
        assert!(conn.queue.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn connection_limit_refuses_excess_connections_and_recovers_after_close() {
        let conns = Connections::default();
        let mut clients = Vec::new();
        for id in 0..MAX_CONNECTIONS as u64 {
            let (server, client) = UnixStream::pair().unwrap();
            conns.register(id, server).unwrap();
            clients.push(client);
        }
        let (server, _client) = UnixStream::pair().unwrap();
        assert!(conns.register(MAX_CONNECTIONS as u64, server).is_err());
        conns.unregister(0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while *conns.writers.count.lock().unwrap() == MAX_CONNECTIONS {
            assert!(Instant::now() < deadline, "closed writer did not exit");
            std::thread::yield_now();
        }
        let (server, _client) = UnixStream::pair().unwrap();
        conns.register(MAX_CONNECTIONS as u64, server).unwrap();
        conns.finish(deadline);
    }

    #[test]
    fn finish_aborts_blocked_registered_and_retiring_writers_at_the_deadline() {
        let conns = Connections::default();
        let mut clients = Vec::new();
        for id in 0..2 {
            let (server, client) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            conns.register(id, server).unwrap();
            conns.send(id, "x".repeat(8 << 20));
            clients.push(client);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = conns.state.lock().unwrap();
            if state
                .conns
                .values()
                .all(|conn| conn.queue.lock().unwrap().in_flight)
            {
                break;
            }
            assert!(Instant::now() < deadline, "writers never started");
            drop(state);
            std::thread::yield_now();
        }
        // A reader that ended may still have a blocked writer to abort.
        conns.unregister(0);
        let start = Instant::now();
        conns.finish(start + Duration::from_millis(30));
        assert!(start.elapsed() < Duration::from_secs(1));
        for mut client in clients {
            let mut received = Vec::new();
            client
                .read_to_end(&mut received)
                .expect("aborted writer closes the socket");
            assert!(
                received.len() < 8 << 20,
                "writer should be cut off mid-frame"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while *conns.writers.count.lock().unwrap() != 0 {
            assert!(Instant::now() < deadline, "aborted writers did not exit");
            std::thread::yield_now();
        }
    }

    /// The deadline covers the whole response: a client that keeps reading
    /// a trickle (so every individual write call makes progress) is still
    /// cut off once the response's overall deadline passes.
    #[test]
    fn a_trickle_reader_cannot_renew_the_response_deadline() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 512];
                while !stop.load(Ordering::Relaxed) {
                    if matches!(client.read(&mut buf), Ok(0) | Err(_)) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        let deadline = Duration::from_millis(300);
        let start = Instant::now();
        let written = write_within(&server, &vec![b'x'; 32 << 20], deadline);
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        drop(server);
        reader.join().unwrap();
        assert!(!written, "a 32 MiB trickle cannot finish in {deadline:?}");
        assert!(
            elapsed < deadline + Duration::from_millis(500),
            "cut off at the overall deadline, not renewed per write: {elapsed:?}"
        );
    }

    #[test]
    fn a_prompt_reader_gets_large_frames_whole_and_in_order() {
        let (server, client) = UnixStream::pair().unwrap();
        let conns = Connections::default();
        conns.register(7, server).unwrap();
        let frames: Vec<String> = (0..3)
            .map(|n| format!("{}{n}\n", "y".repeat(3 << 20)))
            .collect();
        for frame in &frames {
            conns.send(7, frame.clone());
        }
        let mut reader = std::io::BufReader::new(client);
        for frame in &frames {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            assert_eq!(&line, frame);
        }
        conns.finish(Instant::now() + Duration::from_secs(5));
    }

    /// Dropping a registry without `finish` still releases every writer
    /// thread and its socket.
    #[test]
    fn dropping_the_registry_releases_writers_and_sockets() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let conns = Connections::default();
        conns.register(1, server).unwrap();
        conns.send(1, "queued\n".into());
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        drop(conns);
        let mut received = String::new();
        client
            .read_to_string(&mut received)
            .expect("EOF once the writer exits and drops the socket");
        assert_eq!(received, "queued\n");
    }
}
