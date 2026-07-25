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
use djinn_k8s::warm_job::{WARM_COMMAND_BIN, build_warm_job};

use crate::Cli;

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
