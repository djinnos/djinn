//! **`authority-cutover`** — run the launcher-authority cutover, or its reverse.
//!
//! A thin shell over [`djinn_server::authority_cutover::run`], mirroring
//! `djinn-k8s`'s `bin/cutover-preflight.rs`: it parses nothing it can delegate,
//! decides nothing, and holds no copy of any rule. Everything the cutover
//! *does* — the step ordering, both drain checks, the real deploy-time
//! preflight, the compare-and-swap and the reverse that can refuse itself —
//! belongs to `ResizeRollout`, which this binary composes through
//! `ResizeRollout::production` and cannot reach past.
//!
//! # Invocation
//!
//! `argv[1]` is a LIVE `helm template` render converted to JSON, exactly as
//! `cutover-preflight` takes it, because the preflight this binary runs judges
//! that render. `deploy/cutover/authority-cutover.sh` produces it and re-execs
//! this binary under `env -i` with the rendered `DJINN_K8S_*` — so the verdict
//! depends on the deployment, never on the operator's shell.
//!
//! ```text
//! DJINN_CUTOVER_DIRECTION=activate|rollback   which way (required, never defaulted)
//! DJINN_CUTOVER_PLAN=<plan.json>              retained set, probe, epoch, registry (required)
//! DJINN_CUTOVER_AUTHORITY_MODE=<mode>         the mode the preflight judges (must match direction)
//! DJINN_DATABASE_URL=<postgres url>           the durable half of everything (required)
//! DJINN_CUTOVER_OBSERVATIONS=<bundle.json>    catalog/birth observations for the preflight
//! DJINN_CUTOVER_PAUSED_BY=<name>              recorded on the pause row
//! ```
//!
//! # Exit status
//!
//! The same triple `cutover-preflight` and `render-gate` speak, so one deploy
//! lane reads all three the same way:
//!
//! * `0` — the mode flipped and admission resumed.
//! * `1` — blocked. The mode did not move. Admission is left paused whenever
//!   the block happened at or after the pause step, and the binary says which.
//! * `2` — unevaluable: a missing plan, an unreadable render, a probe image
//!   that is not in the catalog. Nothing was attempted.

use std::process::ExitCode;
use std::sync::Arc;

use djinn_server::authority_cutover::{CutoverFailure, CutoverRequest, run};

#[allow(clippy::print_stdout, clippy::print_stderr)]
#[tokio::main]
async fn main() -> ExitCode {
    // rustls 0.23 requires an explicit process-level CryptoProvider before any
    // TLS use, exactly as `server/src/main.rs` installs one for the server.
    // This binary needs it for the same reason: it talks TLS to the apiserver
    // (kube client) and to the OCI registry (retention probe, step 5).
    //
    // Without it the process panics — not returns UNEVALUABLE, PANICS — on the
    // first handshake, before any cutover step runs. Observed in production on
    // 2026-08-01 attempting the 3i92 activation:
    //
    //   Could not automatically determine the process-level CryptoProvider
    //   from Rustls crate features.
    //
    // A panic here is worse than a refusal: the wrapper's documented contract
    // is 0 flipped / 1 blocked / 2 unevaluable, and a panic exits 101, which no
    // deploy lane classifies. `djinn-k8s`'s kind/kueue/pod-resize test harnesses
    // already carry this same install for the same reason; the operator entry
    // point was the one caller that never got it.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!(
            "authority-cutover: UNEVALUABLE a rustls CryptoProvider was already installed by \
             another component; refusing rather than running against an unknown TLS provider"
        );
        return ExitCode::from(2);
    }

    match drive().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => ExitCode::from(report(&failure)),
    }
}

/// Print the failure the way a deploy lane greps it, and return its exit code.
#[allow(clippy::print_stderr)]
fn report(failure: &CutoverFailure) -> u8 {
    match failure {
        CutoverFailure::Unevaluable(reason) => {
            eprintln!("authority-cutover: UNEVALUABLE {reason}");
        }
        CutoverFailure::Blocked {
            blocked,
            journal,
            admission_left_paused,
        } => {
            eprintln!("authority-cutover: BLOCKED {blocked}");
            eprintln!(
                "authority-cutover: completed={}",
                journal
                    .iter()
                    .map(|step| format!("{step:?}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            eprintln!(
                "authority-cutover: authority-mode=unchanged admission={}",
                if *admission_left_paused {
                    "PAUSED — resume it deliberately once the block is resolved"
                } else {
                    "untouched"
                }
            );
        }
    }
    failure.exit_code()
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn drive() -> Result<(), CutoverFailure> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| CutoverFailure::Unevaluable(USAGE.to_string()))?;
    if path == "-h" || path == "--help" {
        println!("{USAGE}");
        return Ok(());
    }

    let request = CutoverRequest::from_env(path).map_err(CutoverFailure::Unevaluable)?;

    // The preflight judges the mode named by `DJINN_CUTOVER_AUTHORITY_MODE`,
    // and the flip targets the mode named by `DJINN_CUTOVER_DIRECTION`. If
    // those two disagree the operator would get a `resize-v2` readiness verdict
    // stamped on a `leaf-v1` flip, so the disagreement is refused rather than
    // reconciled by preferring one of them.
    let judged = djinn_k8s::cutover_preflight_driver::authority_mode_from_env()
        .map_err(CutoverFailure::Unevaluable)?;
    if judged != request.direction.target() {
        return Err(CutoverFailure::Unevaluable(format!(
            "DJINN_CUTOVER_AUTHORITY_MODE={} judges a different mode than \
             DJINN_CUTOVER_DIRECTION={} targets ({}); the preflight verdict would not be about \
             the flip",
            judged.as_wire(),
            request.direction.as_str(),
            request.direction.target().as_wire(),
        )));
    }

    let url = std::env::var("DJINN_DATABASE_URL").map_err(|_| {
        CutoverFailure::Unevaluable(
            "DJINN_DATABASE_URL is not set; the drain fence, the catalog and the authority \
             singleton all live there"
                .to_string(),
        )
    })?;
    let db = djinn_db::Database::open_with_config(djinn_db::DatabaseConnectConfig::Postgres(
        djinn_db::PostgresDatabaseConfig { url },
    ))
    .map_err(|error| {
        CutoverFailure::Unevaluable(format!("cannot open DJINN_DATABASE_URL: {error}"))
    })?;

    // The runtime is built from the ambient cluster configuration — the
    // in-cluster ServiceAccount, or the operator's kubeconfig — which is the
    // cluster whose Pods the drain proof must not find.
    let runtime = djinn_k8s::runtime::KubernetesRuntime::new(
        djinn_k8s::config::KubernetesConfig::from_env(),
        Arc::new(djinn_supervisor::ConnectionRegistry::new()),
    )
    .await
    .map_err(|error| CutoverFailure::Unevaluable(format!("cannot reach the apiserver: {error}")))?;

    // The pause this cutover writes is a durable row; the bus is how repositories
    // announce it to in-process subscribers, of which a one-shot deploy binary
    // has none. A sink, not a stub: nothing in the cutover reads an event back.
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    let report = run(
        db,
        djinn_server::events::event_bus_for(&tx),
        Arc::new(runtime),
        &request,
    )
    .await?;

    println!(
        "authority-cutover: FLIPPED direction={} epoch={} completed={}",
        request.direction.as_str(),
        report.epoch,
        report
            .journal
            .iter()
            .map(|step| format!("{step:?}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    println!(
        "authority-cutover: pods-created-while-paused={}",
        report.dispatches_admitted_while_paused
    );
    Ok(())
}

const USAGE: &str = "usage: authority-cutover <rendered-manifests.json>\n\
     env: DJINN_CUTOVER_DIRECTION={activate|rollback} DJINN_CUTOVER_PLAN=<plan.json>\n\
     \x20    DJINN_CUTOVER_AUTHORITY_MODE={leaf-v1|resize-v2} DJINN_DATABASE_URL=<postgres url>\n\
     \x20    [DJINN_CUTOVER_OBSERVATIONS=<bundle.json>] [DJINN_CUTOVER_PAUSED_BY=<name>]\n\
     exit: 0 flipped, 1 blocked, 2 unevaluable\n\
     prefer the wrapper: deploy/cutover/authority-cutover.sh <chart-dir> [helm args...]";
