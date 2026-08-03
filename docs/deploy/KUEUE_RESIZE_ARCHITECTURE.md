# Kueue admission and invocation resize architecture

This diagram is the operator/developer map for how Djinn turns durable work into Kubernetes Jobs,
shares one finite node across task-runs, graph warming, and standalone SCIP indexing, and temporarily
lifts CPU for a fenced build invocation.

```mermaid
flowchart TB
    subgraph Control["Djinn control plane"]
        Board["Board / durable task state"]
        Dispatch["Coordinator dispatch\nselect role + generation"]
        Render["Render suspended Job\nactual CPU / memory requests\nqueue-name + generation identity"]
        Mirror["Mirror fetcher\nobserves repository head"]
        WarmScheduler["Graph warmer\ndurable warm attempt"]
        ScipScheduler["SCIP scheduler\nchanged head + quiescence\n3h cadence + retry ledger"]

        Board --> Dispatch --> Render
        Mirror --> WarmScheduler
        Mirror --> ScipScheduler
    end

    subgraph Capacity["Dynamic capacity authority (djinn-server)"]
        Node["Eligible Node allocatable\nCPU + memory + Pod ceiling"]
        Protected["Live protected Pods only\nserver, Postgres, Qdrant, Zot, BuildKit\nplus active protected background work"]
        Headroom["Operator headroom\n750m CPU + 1 GiB memory"]
        Derive["Capacity controller\nallocatable − protected − headroom"]
        CQ["ClusterQueue resource vector\nCPU + memory: dynamic fit limits\nPods: real post-reserve Node ceiling"]

        Node --> Derive
        Protected --> Derive
        Headroom --> Derive
        Derive -->|"fenced JSON Patch"| CQ
    end

    subgraph Kueue["Kueue — one shared cohort, three entry queues"]
        TaskQ["djinn-kueue\ntask-run Jobs"]
        WarmQ["djinn-warm\ngraph warm Jobs"]
        ScipQ["djinn-scip\nstandalone semantic index Jobs"]
        WL["Workload\nactual PodSet requests"]
        Fit{"Fits unused\nCPU + memory + Pods?"}
        Pending["Remain suspended\nexplicit pending reason"]
        Admit["Reserve quota + admit\nunsuspend owning Job"]

        TaskQ --> WL
        WarmQ --> WL
        ScipQ --> WL
        CQ --> Fit
        WL --> Fit
        Fit -->|No| Pending
        Pending -->|"re-evaluate on quota release"| Fit
        Fit -->|Yes| Admit
    end

    Render --> TaskQ
    WarmScheduler -->|"only one warm attempt"| WarmQ
    ScipScheduler -->|"no warm in flight; prior warm tried\nhead not already indexed"| ScipQ

    subgraph TaskPod["Admitted task-run Pod"]
        Start["Pod starts at rendered birth request"]
        Worker["Agent worker / planner / reviewer"]
        Build{"Command needs\nbuild CPU?"}
        LightCommand["Run non-build command\nwithout invocation lease"]
        Lease["Acquire weighted FIFO invocation lease\nconsumer=task_invocation\nenqueue sequence + fencing token"]
        Permit["Read immutable Pod permit\nTaskRun + generation + Pod UID\nadmitted CPU ceiling + protocol"]
        Authorize{"Resize authority permits lift?\nmode=Enforce\nprotocol=resize-v2\nUID + generation + token match"}
        Patch["PATCH Pod CPU request/limit\nup to its own admitted ceiling"]
        Confirm["Poll kubelet status\nconfirm resize actually applied"]
        Lift["Launcher lifts invocation cpu.max"]
        Command["Run compile / test command"]
        Drop["Drop cpu.max to birth limit\nwait_empty / cgroup.kill as required"]
        Release["Terminalize invocation lease\nrelease weighted occupancy"]
        Refuse["No lift\nrun clamped or return fenced refusal"]

        Start --> Worker --> Build
        Build -->|No| LightCommand
        Build -->|Yes| Lease --> Permit --> Authorize
        Authorize -->|No| Refuse
        Authorize -->|Yes| Patch --> Confirm --> Lift --> Command
        Command --> Drop --> Release
    end

    Admit -->|task-run| Start
    Release -->|"CPU occupancy returned"| Fit

    subgraph Background["Background graph pipeline"]
        WarmPod["Warm Pod\nclone + normalize mtimes\ncompile shared Cargo target base\nrun graph pipeline"]
        Claim["Cross-Pod semantic index claim\nshared-cache flock"]
        Publish["Publish canonical graph\nrecord durable warm outcome"]
        ScipPod["SCIP Pod\nrequires warm Cargo base\nfill content-addressed semantic cache"]
        Indexed{"Produced index\nfor exact revision?"}
        Success["Job succeeds\nrevision becomes indexed ledger"]
        Failed["Job fails without claiming revision\nretry allowed after cadence"]

        WarmPod --> Claim --> Publish
        ScipPod --> Claim --> Indexed
        Indexed -->|Yes| Success
        Indexed -->|cold base / claim held / error| Failed
        Failed --> ScipScheduler
        Publish --> ScipScheduler
    end

    Admit -->|warm| WarmPod
    Admit -->|SCIP| ScipPod
    Publish -->|"warm quota returned"| Fit
    Success -->|"SCIP quota returned"| Fit
    Failed -->|"terminal Pod releases quota"| Fit
```

## Reading the diagram

### Admission and capacity

Kueue is the authority that decides whether a Kubernetes Job may start. Djinn always renders the Job
with its real resource requests; Kueue creates a Workload and keeps the Job suspended until that PodSet
fits the shared ClusterQueue.

The capacity controller does not set a fixed concurrency number. For every eligible Node it derives:

```text
admissible vector = allocatable − live protected Pod requests − configured headroom
```

CPU and memory therefore limit work according to each Job's actual shape. The Pods dimension is only the
real post-reserve Kubernetes Pod ceiling; it is not a second build-shaped concurrency limit. Terminal
`Succeeded` and `Failed` protected Pods consume no resources and must not be included.

### Task-run versus invocation capacity

Kueue admits the whole task-run Pod. Inside that admitted Pod, a command that actually needs build CPU
uses the retained invocation lease. The two layers answer different questions:

- **Kueue:** may this Pod exist on the node now?
- **Invocation lease + resize:** may this exact, fenced command temporarily lift this Pod from its birth
  CPU request to the Pod's already-admitted ceiling?

The resize path fails closed. A stale generation, wrong Pod UID, wrong launcher protocol, disarmed
authority, missing permit, stale fencing token, rejected PATCH, or unconfirmed kubelet status cannot lift
`cpu.max`.

### Warm and SCIP coordination

The warm Job owns canonical graph publication and creates the reusable Cargo target base. The standalone
SCIP Job fills the semantic-index artifact cache for an exact repository revision; it never publishes the
served graph itself.

They do not run the expensive semantic phase concurrently for one project:

- a nonterminal warm Job blocks SCIP dispatch;
- both take the same cross-Pod semantic-index claim;
- a cold-base or contention skip makes SCIP fail, so it cannot falsely mark the revision indexed;
- the failed attempt consumes the cadence floor to prevent a crash loop, but remains retryable later;
- a successful exact-revision Job is the retained change-detection ledger.

### Quota release

Quota is returned when the Workload finishes. Separately, the dynamic capacity controller excludes
terminal protected Pods even while Kubernetes retains their API objects for logs or garbage collection.
This prevents a failed SCIP or warm Pod from shrinking the ClusterQueue and blocking its own recovery.
