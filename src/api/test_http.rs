//! Synthetic HTTP peers with bounded lifetimes, shared by transport and
//! serve-loop regression tests. Never talks to Figma or uses real files.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) const TEST_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const RESPONSE_BOUND: Duration = Duration::from_secs(2);
const PEER_LIFETIME: Duration = Duration::from_secs(4);

pub(crate) enum Reply {
    Complete(&'static str),
    /// Send a partial response, then keep the connection open.
    Stall(&'static str),
    /// Send a partial response, then a byte every 25ms.
    Trickle(&'static str),
}

pub(crate) struct Peer {
    pub(crate) url: String,
    pub(crate) requests: Receiver<String>,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Peer {
    pub(crate) fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = mpsc::channel();
        let (seen, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            for reply in replies {
                // Serve tests open a store before starting their loop;
                // allow startup time independently of the stalled reply.
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline
                                || stopped.recv_timeout(Duration::from_millis(10))
                                    != Err(mpsc::RecvTimeoutError::Timeout)
                            {
                                return;
                            }
                        }
                        Err(e) => panic!("accept synthetic HTTP request: {e}"),
                    }
                };
                stream.set_read_timeout(Some(PEER_LIFETIME)).unwrap();
                stream.set_write_timeout(Some(RESPONSE_BOUND)).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        return;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16384, "unexpectedly large test request");
                }
                let _ = seen.send(String::from_utf8(request).unwrap());
                let response = match reply {
                    Reply::Complete(s) | Reply::Stall(s) | Reply::Trickle(s) => s,
                };
                if stream.write_all(response.as_bytes()).is_err() {
                    continue;
                }
                match reply {
                    Reply::Complete(_) => {}
                    Reply::Stall(_) => {
                        let _ = stopped.recv_timeout(PEER_LIFETIME);
                    }
                    Reply::Trickle(_) => {
                        let end = Instant::now() + PEER_LIFETIME;
                        while Instant::now() < end
                            && stopped.recv_timeout(Duration::from_millis(25))
                                == Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            if stream.write_all(b" ").is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.thread.take().unwrap().join().unwrap();
    }
}
