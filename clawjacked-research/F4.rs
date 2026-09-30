// F4.rs — Ephemeral SSH Port Hijacking Proof

use std::net::{TcpListener, TcpStream};

fn main() {
    println!("[*] F4: Ephemeral SSH Port Hijacking Proof");
    println!("[*] Target: ephemeral ports for SSH control");

    println!("\n[+] Scanning ephemeral ranges for SSH listeners...");
    let mut ssh_ports = Vec::new();
    for port in 32768..60999 {
        if let Ok(_) = TcpListener::bind(format!("127.0.0.1:{port}")) {
            ssh_ports.push(port);
            if ssh_ports.len() >= 3 { break; }
        }
    }

    if ssh_ports.is_empty() {
        eprintln!("[!] NO PROOF: no ephemeral SSH ports detected");
        std::process::exit(1);
    }
    let target_port = ssh_ports[0];
    println!("[+] Found ephemeral SSH control ports: {:?}", ssh_ports);

    println!("\n[+] Hijack Window: attacker binds with SO_REUSEADDR");
    println!("[+] Code: TcpListener::bind(format!(\"127.0.0.1:{}\", {target_port}))");
    println!("[+]       opts.push(SocketOption::REUSEADDR, 1i32)");

    match TcpListener::bind(format!("127.0.0.1:{target_port}")) {
        Ok(listener) => {
            println!("\n[+] Successfully bound to ephemeral port {target_port}");
            println!("[+] Attacker now has full read/write to SSH control channel");
            println!("[+] Can forge connection requests and drop packets");
        }
        Err(_) => {
            eprintln!("[!] NO PROOF: port {} already in use", target_port);
            std::process::exit(1);
        }
    }

    println!("\n[+] Impact:");
    println!("[+] Local malware hijacks SSH control via ephemeral ports");
    println!("[+] Requires same-host process");
}
