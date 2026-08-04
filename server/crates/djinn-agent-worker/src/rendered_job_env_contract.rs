//! The coverage whose absence hid task opsu's P0.
//!
//! # What went wrong, and why nothing caught it
//!
//! Task 9whk (#2513) added `--task-run-pod-uid` to [`crate::WorkerDefaultArgs`]
//! as a REQUIRED argument with no default. Nothing in `djinn-k8s` ever rendered
//! `DJINN_TASK_RUN_POD_UID` into the task-run Job. Because the worker's `command`
//! is `["djinn-agent-worker", "task-run"]` with every value supplied through the
//! container environment, clap had nowhere else to find it, so every task-run Pod
//! in production died in argv parsing (`exit 2`) before a single line of worker
//! code ran.
//!
//! The three suites that touch this binary all passed:
//!   * `djinn-agent-worker`'s integration tests (`rpc_roundtrip`, `in_pod_drive`,
//!     `cancel_path`) spawn the binary themselves and set `DJINN_TASK_RUN_POD_UID`
//!     on the command they build — they assert the worker's behaviour GIVEN a
//!     satisfied argv, never that anything supplies it;
//!   * `djinn-k8s`'s `job.rs` unit tests assert the env vars someone remembered to
//!     name, one `assert_eq!` per variable — a hand-written list, which is exactly
//!     what rots when the binary grows a new requirement;
//!   * nothing anywhere compared the two.
//!
//! # What this file asserts
//!
//! The join neither side could make alone: it lives in the binary crate, so it can
//! read the REAL clap definition, and it dev-depends on `djinn-k8s`, so it can
//! render the REAL production manifest. The required set is DERIVED from clap on
//! every run — adding a required argument to `WorkerDefaultArgs` fails this test
//! until the renderer supplies it, with no list to remember to update.
//!
//! Both entry points the cluster actually launches are covered: the env-driven
//! `task-run` Job and the argv-driven `warm-graph` Job.

use std::collections::BTreeMap;

use clap::{CommandFactory, Parser};
use uuid::Uuid;

use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::warm_job::{WARM_COMMAND_BIN, build_warm_job, stamp_warm_attempt};

use crate::{Cli, REQUIRED_WARM_GRAPH_ENVIRONMENT};

/// An argument the process cannot start without: clap marked it required and no
/// default value can stand in for a missing one.
fn must_be_supplied(arg: &clap::Arg) -> bool {
    arg.is_required_set() && arg.get_default_values().is_empty()
}

/// The arguments of one subcommand that a launcher is obliged to supply,
/// straight out of the parser the binary really runs.
fn unsatisfiable_without(subcommand: &str) -> Vec<clap::Arg> {
    let cli = Cli::command();
    let sub = cli
        .get_subcommands()
        .find(|sub| sub.get_name() == subcommand)
        .unwrap_or_else(|| {
            panic!("djinn-agent-worker has no `{subcommand}` subcommand; a renderer launches it")
        });
    sub.get_arguments()
        .filter(|arg| must_be_supplied(arg))
        .cloned()
        .collect()
}

/// **The contract, task-run path.** Every argument the `task-run` subcommand
/// cannot start without is reachable from the environment the production Job
/// renders.
///
/// The manifest is walked by field access alone so this file names no Kubernetes
/// types: the k8s capability boundary (`scripts/check-k8s-boundary.sh`) keeps
/// that surface inside `djinn-k8s`, and a rendered manifest is all this needs.
#[test]
fn every_required_task_run_argument_is_rendered_into_the_job() {
    let job = build_task_run_job(
        &KubernetesConfig::for_testing(),
        &Uuid::now_v7(),
        "proj-opsu",
        "djinn-taskrun-opsu",
        "registry.example/djinn-project:opsu",
        &[],
        None,
        false,
        None,
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered task-run Job has a pod spec");
    let worker = pod
        .containers
        .iter()
        .find(|container| container.name == "worker")
        .expect("rendered task-run Job has a worker container");

    // Guard the premise: this test only proves anything while the worker is
    // launched as `task-run` with no flags on the command line.
    assert_eq!(
        worker.command.as_deref(),
        Some([WARM_COMMAND_BIN.to_string(), "task-run".to_string()].as_slice()),
        "the worker container's command changed; this contract assumes every value \
         arrives through the environment"
    );

    // `name -> can this env entry actually deliver a value`. A `valueFrom` entry
    // (downward API, secret, configMap) carries no literal at render time but is
    // resolved by the kubelet before the process starts, so it counts.
    let rendered: BTreeMap<&str, bool> = worker
        .env
        .iter()
        .flatten()
        .map(|entry| {
            let satisfied =
                entry.value.as_deref().is_some_and(|v| !v.is_empty()) || entry.value_from.is_some();
            (entry.name.as_str(), satisfied)
        })
        .collect();

    let required = unsatisfiable_without("task-run");
    // Non-vacuity: if the derivation ever stops seeing clap's required set, this
    // test passes without checking anything — which is the exact failure mode it
    // exists to prevent.
    assert!(
        !required.is_empty(),
        "derived no required task-run arguments from the clap definition; the contract below \
         would pass vacuously"
    );

    for arg in required {
        let id = arg
            .get_long()
            .map(str::to_owned)
            .unwrap_or_else(|| arg.get_id().as_str().to_owned());
        let env = arg.get_env().unwrap_or_else(|| {
            panic!(
                "`--{id}` is required but reads no environment variable. The task-run Pod runs \
                 `djinn-agent-worker task-run` with NO flags, so nothing can ever supply it and \
                 every task-run Pod exits 2 in argv parsing."
            )
        });
        let env = env.to_str().expect("env var names are UTF-8");

        match rendered.get(env) {
            Some(true) => {}
            Some(false) => panic!(
                "the task-run Job renders {env} (for required `--{id}`) with neither a value nor \
                 a valueFrom; the worker sees an empty string and clap rejects it."
            ),
            None => panic!(
                "the task-run Job does not render {env}, which the worker REQUIRES for `--{id}`. \
                 Every task-run Pod dispatched with this manifest exits 2 before running any \
                 worker code. Add it to `build_task_run_env` in djinn-k8s/src/job.rs."
            ),
        }
    }
}

/// **The contract, warm path.** The warm Pod supplies its arguments positionally
/// on a shell command line rather than through the environment, so assert the
/// stronger thing directly: the exact argv the warm Pod `exec`s parses with the
/// binary's real parser, with no environment at all.
#[test]
fn the_warm_pod_argv_parses_with_the_real_parser() {
    let job = build_warm_job(
        &KubernetesConfig::for_testing(),
        "proj-opsu",
        "deadbeef",
        "registry.example/djinn-project:opsu",
        None,
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered warm Job has a pod spec");
    let script = pod.containers[0]
        .command
        .as_deref()
        .and_then(|command| command.last())
        .expect("warm Pod runs a shell script");

    let exec_line = script
        .lines()
        .find(|line| line.starts_with(&format!("exec {WARM_COMMAND_BIN}")))
        .unwrap_or_else(|| panic!("warm Pod script never execs {WARM_COMMAND_BIN}:\n{script}"));

    let argv = split_shell_words(exec_line.trim_start_matches("exec "));
    Cli::try_parse_from(&argv).unwrap_or_else(|error| {
        panic!(
            "the warm Pod's own command line does not parse: {argv:?}\n{error}\nEvery warm Pod \
             dispatched with this manifest exits 2 before warming anything."
        )
    });

    // The argv path only proves the warm subcommand is satisfiable because none
    // of its required arguments hide behind the environment. If that changes,
    // the warm Job's env needs the same treatment as the task-run Job's.
    for arg in unsatisfiable_without("warm-graph") {
        assert!(
            arg.is_positional(),
            "required warm-graph argument `{}` is not positional; the warm Pod's shell \
             command supplies positionals only, so it must be rendered into the Pod env",
            arg.get_id()
        );
    }
}

/// **The contract, SCIP-index path.** Same argument as the warm path, for the
/// standalone semantic-index Pod added alongside it.
///
/// This Pod is dispatched by a scheduler on a 3-hour cadence rather than by an
/// interactive trigger, so a `scip-index` subcommand that did not exist — or
/// whose argv did not parse — would produce a Pod that exits 2, a Job that
/// reports failure, and a change-detection ledger that never advances. That
/// pattern would be invisible in the warm path's own tests, which is precisely
/// why the derivation below reads the real clap parser rather than a list.
#[test]
fn the_scip_index_pod_argv_parses_with_the_real_parser() {
    let job = djinn_k8s::scip_job::build_scip_index_job(
        &KubernetesConfig::for_testing(),
        "proj-scip",
        "registry.example/djinn-project:scip",
        "0123456789abcdef0123456789abcdef01234567",
        None,
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered SCIP Job has a pod spec");
    let script = pod.containers[0]
        .command
        .as_deref()
        .and_then(|command| command.last())
        .expect("SCIP Pod runs a shell script");

    let exec_line = script
        .lines()
        .find(|line| line.starts_with(&format!("exec {WARM_COMMAND_BIN}")))
        .unwrap_or_else(|| panic!("SCIP Pod script never execs {WARM_COMMAND_BIN}:\n{script}"));

    let argv = split_shell_words(exec_line.trim_start_matches("exec "));
    assert_eq!(
        argv.get(1).map(String::as_str),
        Some("scip-index"),
        "the SCIP Pod must invoke the semantic-only subcommand, not the combined \
         warm pipeline — running `warm-graph` here would re-acquire the whole \
         cargo phase this split exists to leave behind: {argv:?}"
    );
    Cli::try_parse_from(&argv).unwrap_or_else(|error| {
        panic!(
            "the SCIP Pod's own command line does not parse: {argv:?}\n{error}\nEvery SCIP \
             Pod dispatched with this manifest exits 2 before indexing anything."
        )
    });

    for arg in unsatisfiable_without("scip-index") {
        assert!(
            arg.is_positional(),
            "required scip-index argument `{}` is not positional; the SCIP Pod's shell \
             command supplies positionals only, so it must be rendered into the Pod env",
            arg.get_id()
        );
    }
}

/// Minimal double-quote-aware tokenizer: enough for the argv this crate's own
/// renderer emits (`exec <bin> warm-graph "<project id>"`), and it deliberately
/// refuses anything more exotic rather than guessing at shell semantics.
fn split_shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '$' | '`' | '\\' | '\'' => panic!(
                "warm Pod argv contains shell metacharacters this contract will not interpret: \
                 {line}"
            ),
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    assert!(!quoted, "unbalanced quote in warm Pod argv: {line}");
    if started {
        words.push(current);
    }
    words
}

/// **The contract, warm build-lease path.** The in-Pod worker hands its build
/// slot back at the cargo→graph boundary using two values it can only get from
/// the environment. Nothing but this test compares the names the worker reads
/// with the names the renderer writes.
///
/// A drift here is silent and expensive: `WarmBuildLease::from_env`
/// deliberately returns `None` for anything it cannot prove, so a renamed
/// variable does not crash the warm — it just quietly gives up the ~20% of
/// build capacity the release was supposed to recover, on every warm, forever.
/// Exactly the shape of defect this module was created for.
#[test]
fn the_leased_warm_job_renders_the_build_lease_identity_the_worker_releases_with() {
    use djinn_k8s::graph_warmer_identity::LeasedWarmJobIdentity;
    use djinn_k8s::warm_job::{
        ENV_WARM_LEASE_CONSUMER_ID, ENV_WARM_LEASE_FENCING_TOKEN, build_leased_warm_job,
    };

    use crate::warm_build_lease::{ENV_LEASE_CONSUMER_ID, ENV_LEASE_FENCING_TOKEN, WarmBuildLease};

    // Both sides of the contract must agree on the NAMES…
    assert_eq!(ENV_LEASE_CONSUMER_ID, ENV_WARM_LEASE_CONSUMER_ID);
    assert_eq!(ENV_LEASE_FENCING_TOKEN, ENV_WARM_LEASE_FENCING_TOKEN);

    let identity = LeasedWarmJobIdentity::new("proj-lease", "warm-req-7", "rev-1", 4242);
    let job = build_leased_warm_job(
        &KubernetesConfig::for_testing(),
        "proj-lease",
        "registry.example/djinn-project:lease",
        None,
        &identity,
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered leased warm Job has a pod spec");
    let warmer = pod
        .containers
        .iter()
        .find(|container| container.name == "warmer")
        .expect("rendered leased warm Job has a warmer container");
    let rendered: BTreeMap<&str, &str> = warmer
        .env
        .iter()
        .flatten()
        .filter_map(|entry| Some((entry.name.as_str(), entry.value.as_deref()?)))
        .collect();

    // …and the RENDERED VALUES must be exactly what the ledger will fence on.
    assert_eq!(
        rendered.get(ENV_LEASE_CONSUMER_ID).copied(),
        Some("warm-req-7"),
        "the warmer container must carry the durable lease consumer id"
    );
    assert_eq!(
        rendered.get(ENV_LEASE_FENCING_TOKEN).copied(),
        Some("4242"),
        "the warmer container must carry the lease's fencing token"
    );

    // The side effect the whole contract exists for: those exact rendered
    // strings resolve into a releasable lease handle.
    let lease = WarmBuildLease::from_parts(
        rendered.get(ENV_LEASE_CONSUMER_ID).copied(),
        rendered.get(ENV_LEASE_FENCING_TOKEN).copied(),
    )
    .expect(
        "the rendered manifest must produce a releasable build lease; without it the warm \
         silently holds a build slot through the whole SCIP phase",
    );
    assert_eq!(lease.key().consumer_id, "warm-req-7");
}

/// An UNLEASED warm holds no durable slot, so it must not be handed an identity
/// it could release. Guards the other direction of the same contract.
#[test]
fn the_unleased_warm_job_renders_no_build_lease_identity() {
    use djinn_k8s::warm_job::{ENV_WARM_LEASE_CONSUMER_ID, ENV_WARM_LEASE_FENCING_TOKEN};

    let job = build_warm_job(
        &KubernetesConfig::for_testing(),
        "proj-unleased",
        "deadbeef",
        "registry.example/djinn-project:unleased",
        None,
    );
    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered warm Job has a pod spec");
    for container in &pod.containers {
        for entry in container.env.iter().flatten() {
            assert_ne!(entry.name, ENV_WARM_LEASE_CONSUMER_ID);
            assert_ne!(entry.name, ENV_WARM_LEASE_FENCING_TOKEN);
        }
    }
}

/// **The contract, warm environment path.** Every environment key the
/// `warm-graph` subcommand cannot start without is present, and non-empty, in
/// the warm Job the production dispatcher actually posts.
///
/// The argv test above is deliberately blind to this: it proves the command
/// line parses, and asserts only that no *clap* requirement hides behind the
/// environment. `warm-graph`'s durable attempt projection is read straight from
/// `std::env::var`, so it never appeared in `Cli::command()` and no assertion
/// on either side of the boundary compared the two spellings.
///
/// That is exactly how this broke. #2941 taught the renderer to project the
/// attempt as `DJINN_WARM_ATTEMPT_ID`; #2942, merged eleven minutes earlier,
/// taught the worker to require `DJINN_WARM_GRAPH_ATTEMPT_ID`. Both suites
/// passed — each asserted its own constant — and every warm Pod for the next
/// day and a half exited before touching cargo or an indexer, with the graph
/// frozen at the deploy that shipped them.
///
/// The manifest here is stamped the way `K8sGraphWarmer` stamps it, so the
/// assertion runs against the bytes the apiserver receives.
#[test]
fn every_required_warm_graph_environment_key_is_rendered_into_the_job() {
    let mut job = build_warm_job(
        &KubernetesConfig::for_testing(),
        "proj-opsu",
        "deadbeef",
        "registry.example/djinn-project:opsu",
        None,
    );
    stamp_warm_attempt(
        &mut job,
        "019fc384-c2d5-7460-aeed-5a168b112b03",
        "2026-08-02T17:30:00Z",
    );

    let pod = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered warm Job has a pod spec");
    let rendered: BTreeMap<&str, &str> = pod
        .containers
        .iter()
        .flat_map(|container| container.env.iter().flatten())
        .filter_map(|entry| Some((entry.name.as_str(), entry.value.as_deref()?)))
        .collect();

    for key in REQUIRED_WARM_GRAPH_ENVIRONMENT {
        let value = rendered.get(key).copied().unwrap_or_else(|| {
            panic!(
                "warm-graph cannot start without `{key}`, and the rendered warm Job does not \
                 project it. Rendered keys: {:?}\nEvery warm Pod dispatched with this manifest \
                 fails closed before warming anything, and the graph stops advancing.",
                rendered.keys().collect::<Vec<_>>()
            )
        });
        assert!(
            !value.trim().is_empty(),
            "warm-graph rejects a blank `{key}`, but the rendered warm Job projects one"
        );
    }
}
