// Shared helpers for the F1-F5 gateway PoCs.
//
// Included from each PoC with `include!("poc_support.rs");` so that every PoC
// stays a single-file program that compiles with a bare `rustc F1.rs`.
//
// Nothing in here is gateway-specific beyond the default port, which follows the
// public OpenClaw documentation (gateway default 18789, HTTP + WS multiplexed).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 18789;

pub struct Args {
    pub host: String,
    pub port: u16,
    pub json: bool,
    pub config: String,
    pub sign: bool,
    pub low: u16,
    pub high: u16,
}

impl Args {
    pub fn parse() -> Args {
        let mut args = Args {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            json: false,
            config: "/tmp/lab-gateway.json".to_string(),
            sign: false,
            low: 32768,
            high: 60999,
        };
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < argv.len() {
            let cur = argv[i].clone();
            let take = |args: &mut Args, i: &mut usize, f: &dyn Fn(&mut Args, &str)| {
                *i += 1;
                if let Some(v) = argv.get(*i) {
                    f(args, v);
                }
            };
            match cur.as_str() {
                "--host" => take(&mut args, &mut i, &|a, v| a.host = v.to_string()),
                "--port" => take(&mut args, &mut i, &|a, v| {
                    a.port = v.parse().unwrap_or(DEFAULT_PORT)
                }),
                "--config" => take(&mut args, &mut i, &|a, v| a.config = v.to_string()),
                "--low" => take(&mut args, &mut i, &|a, v| {
                    a.low = v.parse().unwrap_or(32768)
                }),
                "--high" => take(&mut args, &mut i, &|a, v| {
                    a.high = v.parse().unwrap_or(60999)
                }),
                "--json" => args.json = true,
                "--sign" => args.sign = true,
                "--help" | "-h" => {
                    usage();
                    std::process::exit(0);
                }
                other => {
                    if let Some(v) = other.strip_prefix("--port=") {
                        args.port = v.parse().unwrap_or(DEFAULT_PORT);
                    } else if let Some(v) = other.strip_prefix("--host=") {
                        args.host = v.to_string();
                    } else if let Some(v) = other.strip_prefix("--config=") {
                        args.config = v.to_string();
                    }
                }
            }
            i += 1;
        }
        args
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub fn usage() {
    println!("usage: <poc> [--host HOST] [--port PORT] [--json]");
    println!("  --host HOST    gateway host (default {DEFAULT_HOST})");
    println!("  --port PORT    gateway port (default {DEFAULT_PORT})");
    println!("  --json         machine-readable result on stdout");
    println!("  --config PATH  config file for the hot-reload PoC (F3)");
    println!("  --sign         sign the config payload (F3)");
    println!("  --low/--high   port range to scan (F4)");
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub fn request_full(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<Response> {
    let mut stream = TcpStream::connect((host, port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    Some(parse_response(&buf))
}

pub fn get(host: &str, port: u16, path: &str) -> Option<Response> {
    request_full(host, port, "GET", path, &[], None)
}

pub fn get_bearer(host: &str, port: u16, path: &str, token: &str) -> Option<Response> {
    let auth = format!("Bearer {token}");
    request_full(host, port, "GET", path, &[("Authorization", auth.as_str())], None)
}

pub fn parse_response(buf: &[u8]) -> Response {
    let text = String::from_utf8_lossy(buf).to_string();
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (text.clone(), String::new()),
    };
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Response { status, headers, body }
}

/// Pull a `token=` / `access_token=` value out of a URL fragment.
pub fn token_from_fragment(url: &str) -> Option<String> {
    let fragment = url.split('#').nth(1)?;
    let query = fragment.split_once('?').map(|(_, q)| q).unwrap_or(fragment);
    for pair in query.split(['&', '?']) {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "token" || k == "access_token" {
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub struct WsClient {
    stream: TcpStream,
}

impl WsClient {
    /// Perform the WebSocket upgrade. `extra` carries headers such as Origin.
    pub fn connect(
        host: &str,
        port: u16,
        path: &str,
        extra: &[(&str, &str)],
    ) -> std::io::Result<WsClient> {
        let mut stream = TcpStream::connect((host, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut key_bytes = [0u8; 16];
        for (i, slot) in key_bytes.iter_mut().enumerate() {
            *slot = ((seed >> ((i % 16) * 8)) & 0xFF) as u8 ^ (i as u8).wrapping_mul(31);
        }
        let key = base64(&key_bytes);
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes())?;

        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte)? == 0 {
                break;
            }
            head.push(byte[0]);
            if head.len() > 16384 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head).to_string();
        if !text.starts_with("HTTP/1.1 101") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("upgrade refused: {}", text.lines().next().unwrap_or("")),
            ));
        }
        Ok(WsClient { stream })
    }

    pub fn send_text(&mut self, text: &str) -> std::io::Result<()> {
        let payload = text.as_bytes();
        let mut frame = vec![0x81u8];
        let n = payload.len();
        if n < 126 {
            frame.push(n as u8);
        } else if n < 65536 {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame)
    }

    pub fn recv_text(&mut self, timeout: Duration) -> Option<String> {
        self.stream.set_read_timeout(Some(timeout)).ok()?;
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head).ok()?;
        let opcode = head[0] & 0x0F;
        let mut len = (head[1] & 0x7F) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            self.stream.read_exact(&mut ext).ok()?;
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.stream.read_exact(&mut ext).ok()?;
            len = u64::from_be_bytes(ext);
        }
        let mut payload = vec![0u8; len as usize];
        self.stream.read_exact(&mut payload).ok()?;
        if opcode == 0x8 {
            return None;
        }
        Some(String::from_utf8_lossy(&payload).to_string())
    }
}

/// Lowest port in `[low, high]` that can actually be bound, i.e. is free.
pub fn smallest_free_port(low: u16, high: u16) -> Option<u16> {
    let start = low.max(1024);
    for port in start..=high {
        if TcpListener::bind((DEFAULT_HOST, port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// True when something is already listening on the port.
pub fn port_has_listener(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("{DEFAULT_HOST}:{port}").parse().unwrap(),
        Duration::from_millis(150),
    )
    .is_ok()
}

/// Minimal SHA-1, used only to sign the lab config payload in F3.
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *word = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

pub struct Report {
    pub id: String,
    pub title: String,
    pub confirmed: bool,
    pub evidence: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(id: &str, title: &str) -> Report {
        Report {
            id: id.to_string(),
            title: title.to_string(),
            confirmed: false,
            evidence: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn evidence(&mut self, line: impl Into<String>) {
        self.evidence.push(line.into());
    }

    pub fn note(&mut self, line: impl Into<String>) {
        self.notes.push(line.into());
    }

    pub fn finish(self, json: bool, fail_code: i32) -> ! {
        if json {
            let evidence: Vec<String> =
                self.evidence.iter().map(|e| format!("\"{}\"", escape(e))).collect();
            let notes: Vec<String> =
                self.notes.iter().map(|n| format!("\"{}\"", escape(n))).collect();
            println!(
                "{{\"id\":\"{}\",\"title\":\"{}\",\"confirmed\":{},\"evidence\":[{}],\"notes\":[{}]}}",
                self.id,
                escape(&self.title),
                self.confirmed,
                evidence.join(","),
                notes.join(",")
            );
        } else {
            println!();
            println!("--- {} {} ---", self.id, self.title);
            for line in &self.evidence {
                println!("    {line}");
            }
            for line in &self.notes {
                println!("  i {line}");
            }
            println!(
                "  = {}",
                if self.confirmed {
                    "CONFIRMED against this target"
                } else {
                    "NOT CONFIRMED (see notes)"
                }
            );
        }
        std::process::exit(if self.confirmed { 0 } else { fail_code });
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

pub fn banner(id: &str, title: &str) {
    println!("[*] {id}: {title}");
}
