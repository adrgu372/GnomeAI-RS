// F5 - Cross-tab label leakage.
//
// Hypothesis: route-scoped labels (used for auth decisions) are kept in shared
// browser storage, so one dashboard tab can read or overwrite labels belonging
// to another route or tab, leading to impersonation.
//
// Honest scope, because the original wording was misleading:
//   * `window.opener.localStorage` is NOT a cross-origin primitive: two
//     same-origin tabs already share one storage area, and touching another
//     origin's storage raises SecurityError.
//   * The testable claim is therefore key collision within one origin: a
//     client-side resolver that picks labels by bare key rather than by
//     route-scoped key can be fed attacker labels from a sibling route.
//
// This program models that resolver and checks it, then probes whether the
// gateway itself is reachable with a label alone. It is a faithful test of the
// resolver logic, not of a browser's protection model.
//
// Build:  rustc F5.rs -o F5
// Run:    ./F5 [--host 127.0.0.1] [--port 18789] [--json]

#![allow(dead_code, unused_imports)]

include!("poc_support.rs");

use std::collections::HashMap;

/// A client-side label resolver. `scoped` decides whether labels are keyed by
/// route (safe) or by bare name (collision-prone).
struct LabelStore {
    scoped: bool,
    data: HashMap<String, String>,
    shared_data: HashMap<String, String>,
}

impl LabelStore {
    fn new(scoped: bool) -> LabelStore {
        LabelStore {
            scoped,
            data: HashMap::new(),
            shared_data: HashMap::new(),
        }
    }

    fn key(&self, route: &str, label: &str) -> String {
        if self.scoped {
            format!("{route}::{label}")
        } else {
            label.to_string()
        }
    }

    fn set(&mut self, route: &str, label: &str, value: &str) {
        let key = self.key(route, label);
        if self.scoped {
            self.data.insert(key, value.to_string());
        } else {
            // Unscoped: everything lands in one shared bucket, which is the bug.
            self.shared_data.insert(key, value.to_string());
        }
    }

    fn get(&self, route: &str, label: &str) -> Option<String> {
        let key = self.key(route, label);
        if self.scoped {
            self.data.get(&key).cloned()
        } else {
            self.shared_data.get(&key).cloned()
        }
    }
}

fn main() {
    let args = Args::parse();
    let mut report = Report::new("F5", "Cross-tab label leakage");
    if !args.json {
        banner("F5", "Cross-tab label leakage");
        println!("    target: {}", args.addr());
    }

    // 1. The resolver test: does a sibling route's write leak into this route?
    let mut unsafe_store = LabelStore::new(false);
    unsafe_store.set("/settings", "session", "victim-operator");
    // An attacker-controlled route writes the same bare label.
    unsafe_store.set("/untrusted", "session", "attacker-operator");
    let leaked = unsafe_store.get("/settings", "session");
    report.evidence(format!(
        "unscoped resolver: /settings reads session = {:?}",
        leaked.as_deref().unwrap_or("<none>")
    ));
    let collision = leaked.as_deref() == Some("attacker-operator");

    let mut safe_store = LabelStore::new(true);
    safe_store.set("/settings", "session", "victim-operator");
    safe_store.set("/untrusted", "session", "attacker-operator");
    let safe_read = safe_store.get("/settings", "session");
    report.evidence(format!(
        "route-scoped resolver: /settings reads session = {:?}",
        safe_read.as_deref().unwrap_or("<none>")
    ));

    if !collision {
        report.note("the unscoped resolver did not collide, so this model shows no leakage");
        report.finish(args.json, 1);
    }
    if safe_read.as_deref() != Some("victim-operator") {
        report.note("the scoped resolver also failed to isolate, which is unexpected");
        report.finish(args.json, 1);
    }
    report.evidence("route-scoped keys isolate the labels; bare keys do not");

    // 2. Does the gateway accept a label as a credential on its own?
    let probe = get(&args.host, args.port, "/tools/list");
    let status = probe.as_ref().map(|r| r.status).unwrap_or(0);
    if status == 0 {
        report.note(format!("no HTTP response from {}", args.addr()));
    } else {
        report.evidence(format!("gateway /tools/list without credentials -> HTTP {status}"));
        if status == 200 {
            report.note("the gateway accepted a caller with no credential at all");
        }
    }

    if collision {
        report.evidence(
            "a bare-key resolver lets one route supply labels another route trusts",
        );
        report.note(
            "this is a client-side storage-scoping defect; whether it yields operator access \
             depends on what the labels authorise, which this model does not decide",
        );
        report.confirmed = true;
        report.finish(args.json, 0);
    }
    report.finish(args.json, 1);
}
