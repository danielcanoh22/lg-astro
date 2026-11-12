//! The short-lived, capability-protected LAN server that companion pages on the owner's phone or
//! computer talk to, such as the wallpaper upload page. Each session binds one private local
//! interface, is addressed by a random token and is shown on the TV as a QR code.
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    time::{Duration, Instant},
};

/// Request headers must arrive this soon after the connection opens; a body has its own time.
pub const HEADER_TIME: Duration = Duration::from_secs(5);
/// Connections still sending headers, across the session and from one peer address, so a slow
/// or abusive peer cannot take every slot. A browser opens up to six connections at once.
pub const PENDING_LIMIT: usize = 32;
pub const PENDING_PEER_LIMIT: usize = 8;
const HEADER_LIMIT: usize = 8192;

// Base58 excludes easily confused 0/O and I/l while staying URL-safe.
pub const TOKEN_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

#[derive(Clone, Debug)]
pub struct Session {
    pub url: String,
    pub qr_size: i32,
    pub qr: Vec<bool>,
}

pub fn random_token() -> Result<String, String> {
    let mut token = String::with_capacity(8);
    while token.len() < 8 {
        let mut bytes = [0; 16];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| format!("Secure random source unavailable: {e}"))?;
        // 232 is the largest multiple of 58 below 256, avoiding modulo bias.
        for byte in bytes.into_iter().filter(|byte| *byte < 232) {
            token.push(TOKEN_ALPHABET[(byte % 58) as usize] as char);
            if token.len() == 8 {
                break;
            }
        }
    }
    Ok(token)
}

/// Loopback, or a private or link-local IPv4 peer on the TV's own network.
pub fn local_peer(ip: IpAddr) -> bool {
    ip.is_loopback() || matches!(ip, IpAddr::V4(ip) if ip.is_private() || ip.is_link_local())
}

pub fn listen(loopback_only: bool) -> Result<TcpListener, String> {
    let address = if loopback_only {
        Ipv4Addr::LOCALHOST
    } else {
        if_addrs::get_if_addrs()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|interface| match interface.ip() {
                IpAddr::V4(ip) if ip.is_private() || ip.is_link_local() => Some(ip),
                _ => None,
            })
            .next()
            .ok_or("Connect the TV to your local network, then reopen this dialog")?
    };
    // Bind only the chosen local interface, not every interface or a public address.
    let listener =
        TcpListener::bind((address, 0)).map_err(|e| format!("Local server unavailable: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(listener)
}

pub fn session(address: SocketAddr, token: &str) -> Result<Session, String> {
    let url = format!("http://{address}/{token}/");
    // Highest correction reserves capacity for the small branded center mark.
    let qr = qrcodegen::QrCode::encode_text(&url, qrcodegen::QrCodeEcc::High)
        .map_err(|e| e.to_string())?;
    let size = qr.size();
    Ok(Session {
        url,
        qr_size: size,
        qr: (0..size)
            .flat_map(|y| (0..size).map(move |x| (x, y)))
            .map(|(x, y)| qr.get_module(x, y))
            .collect(),
    })
}

struct Connection {
    stream: TcpStream,
    peer: IpAddr,
    received: Vec<u8>,
    deadline: Instant,
}

/// Connections whose request headers are still arriving. They are read without blocking, so one
/// slow peer cannot hold up requests from others.
#[derive(Default)]
pub struct Pending(Vec<Connection>);

impl Pending {
    /// Drops the connection and returns false when the session or this peer is at its limit.
    pub fn admit(&mut self, stream: TcpStream, peer: IpAddr) -> bool {
        if self.0.len() >= PENDING_LIMIT
            || self.0.iter().filter(|c| c.peer == peer).count() >= PENDING_PEER_LIMIT
            || stream.set_nonblocking(true).is_err()
        {
            return false;
        }
        self.0.push(Connection {
            stream,
            peer,
            received: Vec::new(),
            deadline: Instant::now() + HEADER_TIME,
        });
        true
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Reads what has arrived and returns each connection whose headers are complete, with every
    /// byte received so far, or that failed. A peer that closed early is dropped silently.
    pub fn poll(&mut self) -> Vec<(TcpStream, Result<Vec<u8>, String>)> {
        let mut done = Vec::new();
        let mut index = 0;
        while index < self.0.len() {
            let progress = receive(&mut self.0[index]);
            if matches!(progress, Progress::Waiting) {
                index += 1;
                continue;
            }
            let connection = self.0.swap_remove(index);
            match progress {
                Progress::Arrived => done.push((connection.stream, Ok(connection.received))),
                Progress::Failed(error) => done.push((connection.stream, Err(error))),
                Progress::Waiting | Progress::Closed => {}
            }
        }
        done
    }
}

enum Progress {
    Waiting,
    Arrived,
    Failed(String),
    Closed,
}

fn receive(connection: &mut Connection) -> Progress {
    let mut buffer = [0; 1024];
    let chunk = buffer.len();
    loop {
        // The blank line must end within HEADER_LIMIT bytes, as `request` requires.
        let room = HEADER_LIMIT.saturating_sub(connection.received.len());
        if room == 0 {
            return Progress::Failed("Request headers too large".into());
        }
        match connection.stream.read(&mut buffer[..room.min(chunk)]) {
            Ok(0) => return Progress::Closed,
            Ok(n) => {
                let from = connection.received.len().saturating_sub(3);
                connection.received.extend_from_slice(&buffer[..n]);
                if connection.received[from..]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
                {
                    return Progress::Arrived;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return Progress::Closed,
        }
    }
    if Instant::now() >= connection.deadline {
        Progress::Failed("Request headers timed out".into())
    } else {
        Progress::Waiting
    }
}

/// Whether a request's target is inside the session addressed by `token`.
pub fn carries_token(received: &[u8], token: &str) -> bool {
    let line = received
        .split(|byte| *byte == b'\r')
        .next()
        .unwrap_or_default();
    let target = line.split(|byte| *byte == b' ').nth(1).unwrap_or_default();
    let session = format!("/{token}");
    target == session.as_bytes() || target.starts_with(format!("{session}/").as_bytes())
}

pub fn read_exact(
    reader: &mut impl Read,
    mut bytes: &mut [u8],
    valid: &impl Fn() -> bool,
    deadline: Instant,
) -> Result<(), String> {
    while !bytes.is_empty() {
        if !valid() || Instant::now() >= deadline {
            return Err("Session ended or request timed out".into());
        }
        match reader.read(bytes) {
            Ok(0) => return Err("Incomplete request".into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub length: usize,
}

impl Request {
    /// Whether the request came from another site rather than the page this session served.
    pub fn foreign(&self, host: &str) -> bool {
        self.headers.get("host").map(String::as_str) != Some(host)
            || self
                .headers
                .get("origin")
                .is_some_and(|origin| origin != &format!("http://{host}"))
            || self
                .headers
                .get("sec-fetch-site")
                .is_some_and(|site| site == "cross-site")
    }
}

/// `too_large` is the error for a body over `max_body`, worded for what the page sends.
pub fn request(
    reader: &mut impl Read,
    valid: &impl Fn() -> bool,
    deadline: Instant,
    max_body: usize,
    too_large: &str,
) -> Result<Request, String> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 8192 {
            return Err("Request headers too large".into());
        }
        let mut byte = [0];
        read_exact(reader, &mut byte, valid, deadline)?;
        bytes.push(byte[0]);
    }
    let header = std::str::from_utf8(&bytes).map_err(|_| "Invalid request")?;
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err("Invalid request".into());
    }
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("Invalid header")?;
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("Invalid header".into());
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_owned())
            .is_some()
        {
            return Err("Duplicate header".into());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Chunked uploads are not supported".into());
    }
    let length = headers
        .get("content-length")
        .map(|s| {
            if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
                return Err("Invalid content length");
            }
            s.parse::<usize>().map_err(|_| "Invalid content length")
        })
        .transpose()?
        .unwrap_or(0);
    if length > max_body {
        return Err(too_large.into());
    }
    Ok(Request {
        method: parts[0].into(),
        target: parts[1].into(),
        headers,
        length,
    })
}

pub fn response(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    body: &[u8],
) -> Result<(), String> {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\r\n", body.len())
        .and_then(|_| stream.write_all(body)).map_err(|e| e.to_string())
}

pub fn error_response(stream: &mut TcpStream, status: &str, error: &str) -> Result<(), String> {
    response(
        stream,
        status,
        "application/json; charset=utf-8",
        serde_json::json!({"error":error}).to_string().as_bytes(),
    )
}

/// The QR code points at `/{token}/`; a typed address without the slash lands there too.
pub fn redirect_to_page(stream: &mut TcpStream, token: &str) -> Result<(), String> {
    write!(stream, "HTTP/1.1 303 See Other\r\nLocation: /{token}/\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\r\n")
        .map_err(|error| error.to_string())
}
