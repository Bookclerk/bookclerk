//! Coverage-guided differential of the workerd egress gate
//! (`crates/bookclerk-workerd/bridge/egress.js`) against the Rust
//! `EgressPolicy` (`bookclerk-plugin-manifest`).
//!
//! The input is the JSON wire form of a policy (what the host injects as
//! `EGRESS_POLICY`). `egress.js` runs in an embedded QuickJS context, and
//! its `allowsInitial`, `allowsRedirect` and `allowsTcp` are called for every
//! probe host the shared oracle in `fuzz/oracles/egress_bridge.rs` derives:
//! the JavaScript gate must never allow what the Rust policy refuses, and
//! must agree with it exactly on hostnames. Hosts reach the gate as the
//! oracle's model of workerd's URL parser and of `canonicalSocketHost`
//! prints them (QuickJS has no `URL`); the Node replay checks those models. The replay test in
//! `bookclerk-workerd/tests/fuzz_corpus.rs` and the SDK's
//! `test:egress-bridge` script check the corpus the same way without the
//! embedded engine.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rquickjs::{Context, Ctx, Function, Runtime, Value};

#[path = "../oracles/egress_bridge.rs"]
mod oracle;

thread_local! {
    /// One QuickJS runtime with the bridge loaded, reused for every input:
    /// the gate functions are pure, so no state carries between calls.
    static ENGINE: (Runtime, Context) = {
        let runtime = Runtime::new().expect("QuickJS runtime");
        let context = Context::full(&runtime).expect("QuickJS context");
        context.with(|ctx| {
            ctx.eval::<(), _>(oracle::bridge_script())
                .expect("egress.js loads as a script");
        });
        (runtime, context)
    };
}

/// Calls one gate function and reads its boolean answer.
fn call_gate<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    args: impl rquickjs::function::IntoArgs<'js>,
) -> bool {
    let gate: Function = ctx
        .globals()
        .get(name)
        .unwrap_or_else(|_| panic!("egress.js has no {name}"));
    gate.call::<_, bool>(args)
        .unwrap_or_else(|err| panic!("{name} threw: {err}"))
}

fuzz_target!(|data: &[u8]| {
    let Some(policy) = oracle::decode(data) else {
        return;
    };
    let json = oracle::policy_json(&policy);
    let probes = oracle::probes(&policy);
    ENGINE.with(|(_, context)| {
        context.with(|ctx| {
            let wire: Value = ctx
                .json_parse(json.as_str())
                .expect("policy JSON parses in QuickJS");
            for probe in &probes {
                let js = oracle::Verdicts {
                    initial: probe
                        .fetch
                        .as_deref()
                        .is_some_and(|host| call_gate(&ctx, "allowsInitial", (host, wire.clone()))),
                    redirect: probe.fetch.as_deref().is_some_and(|host| {
                        call_gate(
                            &ctx,
                            "allowsRedirect",
                            (host, probe.hop, wire.clone(), policy.max_redirects),
                        )
                    }),
                    tcp: probe.socket.as_deref().is_some_and(|host| {
                        call_gate(&ctx, "allowsTcp", (host, probe.port, wire.clone()))
                    }),
                };
                oracle::check(&policy, probe, js);
            }
        });
    });
});
