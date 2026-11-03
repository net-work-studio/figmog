//! Synthetic TLS peers. Fragment records below rustls so the test reaches
//! repeated raw reads inside one TLS operation, rather than HTTP reads.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ureq::rustls::{
    self,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

fn certificate() -> CertificateDer<'static> {
    CertificateDer::from(include_bytes!("../../tests/fixtures/https-cert.der").to_vec())
}

pub(crate) fn client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate()).unwrap();
    Arc::new(
        rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

pub(crate) enum Reply {
    Complete(&'static str),
    EncryptedTrickle(&'static str),
    HandshakeTrickle,
}

fn trickle_record(socket: &mut TcpStream, record: &[u8], stopped: &Receiver<()>) {
    if socket.write_all(&record[..5]).is_err() {
        return;
    }
    let end = Instant::now() + Duration::from_secs(3);
    for byte in &record[5..] {
        if Instant::now() >= end
            || stopped.recv_timeout(Duration::from_millis(25))
                != Err(mpsc::RecvTimeoutError::Timeout)
            || socket.write_all(&[*byte]).is_err()
        {
            break;
        }
    }
}

pub(crate) struct TlsPeer {
    pub(crate) url: String,
    pub(crate) requests: Receiver<String>,
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl TlsPeer {
    pub(crate) fn new(replies: Vec<Reply>) -> Self {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            include_bytes!("../../tests/fixtures/https-key.der").to_vec(),
        ));
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(
                rustls::crypto::ring::default_provider().into(),
            )
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate()], key)
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let (stop, stopped) = mpsc::channel();
        let (seen, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline
                                || stopped.recv_timeout(Duration::from_millis(10))
                                    != Err(mpsc::RecvTimeoutError::Timeout)
                            {
                                return;
                            }
                        }
                        Err(e) => panic!("accept synthetic HTTPS request: {e}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                if matches!(reply, Reply::HandshakeTrickle) {
                    let mut hello = [0; 4096];
                    if socket.read(&mut hello).is_err() {
                        return;
                    }
                    let _ = seen.send("TLS handshake started".into());
                    // Claim a 16KiB handshake record and deliver only a
                    // fragment, keeping rustls inside its handshake read.
                    let mut record = vec![0; 16389];
                    record[..5].copy_from_slice(&[22, 3, 3, 0x40, 0]);
                    trickle_record(&mut socket, &record, &stopped);
                    continue;
                }
                let connection = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut tls = rustls::StreamOwned::new(connection, socket);
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if tls.read_exact(&mut byte).is_err() {
                        return;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16384);
                }
                let _ = seen.send(String::from_utf8(request).unwrap());
                let response = match reply {
                    Reply::Complete(s) | Reply::EncryptedTrickle(s) => s,
                    Reply::HandshakeTrickle => unreachable!("handled before TLS setup"),
                };
                if tls.write_all(response.as_bytes()).is_err() || tls.flush().is_err() {
                    return;
                }
                if matches!(reply, Reply::Complete(_)) {
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                    continue;
                }
                tls.conn.writer().write_all(&vec![b' '; 4096]).unwrap();
                let mut record = Vec::new();
                tls.conn.write_tls(&mut record).unwrap();
                assert!(record.len() > 4096);
                // Deliver the encrypted record header, then fragment its
                // ciphertext. No plaintext body bytes can be decrypted
                // until this entire record has arrived.
                trickle_record(&mut tls.sock, &record, &stopped);
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for TlsPeer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.worker.take().unwrap().join().unwrap();
    }
}
