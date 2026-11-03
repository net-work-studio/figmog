//! Shared REST/image HTTP transport. Each client carries its request
//! budget and TLS trust configuration; no Figma token is stored here.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ureq::{ReadWrite, TlsConnector, rustls};
use url::Url;

use super::{ApiError, error_from_status};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
// Match ureq 2's default limit: at most five requests in a redirect chain.
const MAX_REQUESTS: usize = 5;

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTP request deadline exceeded"))
}

/// Lives *below* rustls: one TLS operation may perform arbitrarily many
/// socket reads while assembling a handshake or encrypted record. Every
/// raw operation must use the remaining absolute budget, not a renewed
/// inactivity timeout inherited from ureq's outer response reader.
#[derive(Debug)]
struct DeadlineIo {
    io: Box<dyn ReadWrite>,
    deadline: Instant,
}

impl DeadlineIo {
    fn prepare(&self, read: bool) -> io::Result<()> {
        let budget = remaining(self.deadline)?;
        let socket = self.io.socket().ok_or_else(|| {
            io::Error::other("TLS transport has no socket for deadline enforcement")
        })?;
        if read {
            socket.set_read_timeout(Some(budget))
        } else {
            socket.set_write_timeout(Some(budget))
        }
    }
}

fn normalize_timeout(error: io::Error) -> io::Error {
    // Unix socket timeouts may surface as WouldBlock. Preserve TimedOut
    // through rustls and JSON parsing so callers keep network backoff.
    if error.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(io::ErrorKind::TimedOut, error)
    } else {
        error
    }
}

impl Read for DeadlineIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.prepare(true)?;
        self.io.read(buf).map_err(normalize_timeout)
    }
}

impl Write for DeadlineIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.prepare(false)?;
        self.io.write(buf).map_err(normalize_timeout)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.prepare(false)?;
        self.io.flush().map_err(normalize_timeout)
    }
}

impl ReadWrite for DeadlineIo {
    fn socket(&self) -> Option<&TcpStream> {
        self.io.socket()
    }
}

struct DeadlineTls {
    config: Arc<rustls::ClientConfig>,
    deadline: Instant,
}

impl TlsConnector for DeadlineTls {
    fn connect(
        &self,
        name: &str,
        io: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        self.config.connect(
            name,
            Box::new(DeadlineIo {
                io,
                deadline: self.deadline,
            }),
        )
    }
}

pub(super) struct Client {
    pub(super) timeout: Duration,
    pub(super) tls: Arc<rustls::ClientConfig>,
}

impl Client {
    pub(super) fn new(timeout: Duration) -> Self {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .expect("ring supports TLS 1.2 and 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Self {
            timeout,
            tls: Arc::new(tls),
        }
    }

    pub(super) fn get(&self, url: &str, token: Option<&str>) -> Result<ureq::Response, ApiError> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(|| ApiError::Network("HTTP request deadline overflowed".into()))?;
        let mut url = Url::parse(url).map_err(|error| ApiError::Network(error.to_string()))?;
        for hop in 0..MAX_REQUESTS {
            let budget =
                remaining(deadline).map_err(|error| ApiError::Network(error.to_string()))?;
            // A fresh agent per hop prevents pooling a TLS stream that
            // carries an earlier request's absolute deadline. ureq 2's
            // automatic redirects reuse a full connect timeout instead
            // of clamping it to the request's remaining budget.
            let agent = ureq::builder()
                .redirects(0)
                .timeout_connect(CONNECT_TIMEOUT.min(budget))
                .tls_connector(Arc::new(DeadlineTls {
                    config: self.tls.clone(),
                    deadline,
                }))
                .build();
            let mut request = agent.get(url.as_str()).timeout(budget);
            if let Some(token) = token {
                request = request.set("X-Figma-Token", token);
            }
            let response = match request.call() {
                Ok(response) | Err(ureq::Error::Status(_, response)) => response,
                Err(error) => return Err(ApiError::Network(error.to_string())),
            };
            if matches!(response.status(), 301 | 302 | 303 | 307 | 308)
                && let Some(location) = response.header("Location")
            {
                if hop + 1 == MAX_REQUESTS {
                    return Err(ApiError::Network("reached max redirects (5)".into()));
                }
                url = url
                    .join(location)
                    .map_err(|error| ApiError::Network(error.to_string()))?;
                continue;
            }
            let status = response.status();
            if status >= 400 {
                let retry = response.header("Retry-After").map(str::to_string);
                let message = response.into_string().unwrap_or_default();
                return Err(error_from_status(status, retry.as_deref(), message));
            }
            return Ok(response);
        }
        unreachable!("the final redirect returns an error")
    }
}
