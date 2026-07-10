import { Github, GitMerge, Check } from "lucide-react";
import HeroActions from "../components/HeroActions";
import Logo from "../components/Logo";
import Footer from "../components/Footer";
import LightboxImage from "../components/Lightbox";

/* ————————————————————————————————————————————————————————————
   The page is styled as a proposal moving through djinn's own
   pipeline: header → review & sign-off → build log → merged.
   Mono = system chrome. Serif = the human voice.
———————————————————————————————————————————————————————————— */

const PIPELINE = [
  {
    n: "01",
    name: "Propose",
    status: { label: "spec written", tone: "pass" },
    body: "Anyone on the team writes a proposal: a problem, a goal, acceptance criteria. A living spec that can target one repo or many. Djinn helps draft and refine it in a proposal-scoped chat.",
  },
  {
    n: "02",
    name: "Review & sign off",
    status: { label: "2 sign-offs", tone: "pass" },
    body: "Product and engineering leave feedback; the spec evolves revision by revision. Sign-offs go stale if it changes afterward, so approval always means this exact version.",
  },
  {
    n: "03",
    name: "Build",
    status: { label: "agents running", tone: "warn" },
    body: (
      <>Graduation turns the spec into epics and tasks. Agents work in parallel, each in its own isolated <code className="tick">Kubernetes Job</code>, in a per-project <code className="tick">devcontainer</code>, using the models you configured: per user, per project, per role.</>
    ),
    log: [
      ["$", "kubectl get jobs -n djinn"],
      [" ", "djinn-taskrun-9f2c   Running    api: add usage rollup"],
      [" ", "djinn-taskrun-c41a   Running    ui: spend by proposal"],
      [" ", "djinn-taskrun-77b0   Complete   db: attribution schema"],
    ],
  },
  {
    n: "04",
    name: "Merge",
    status: { label: "checks passed", tone: "pass" },
    body: "An AI reviewer judges every result against the acceptance criteria; weak work loops back. What passes becomes a pull request. You review, you merge. Nothing ships without you.",
  },
];

const CRITERIA = [
  {
    title: "Specs, not prompts",
    body: "Feedback threads, revision history, stale-aware sign-offs. The team argues before the tokens burn, and can freeze, rework, and re-graduate mid-build.",
  },
  {
    title: "Your cloud",
    body: (
      <>Self-hosted on any Kubernetes; a single VPS running <code className="tick">k3s</code> is enough. One <code className="tick">Helm</code> chart bundles Postgres, Qdrant, and the registry on one box, or plugs into EKS / GKE / AKS.</>
    ),
  },
  {
    title: "Your models",
    body: "Anthropic, OpenAI, Google, Bedrock, Vertex, Azure, Copilot, Codex, or any OpenAI-compatible endpoint. One model codes, another reviews. Credentials encrypted, per user.",
  },
  {
    title: "Parallel by default",
    body: (
      <>Every task runs as an isolated <code className="tick">Kubernetes Job</code> in its own <code className="tick">git</code> workspace. The coordinator dispatches by priority and dependency order, capped per user and per model.</>
    ),
  },
  {
    title: "Multi-project, multi-user",
    body: "Each repo gets its own devcontainer image, code graph, and knowledge base. Each teammate brings their own credentials and limits. One proposal can span many repos.",
  },
  {
    title: "Review built in",
    body: "AI reviewers check each change against the proposal's acceptance criteria and send weak work back. You get a clean pull request, and the final say.",
  },
];

function StatusChip({ label, tone }: { label: string; tone: string }) {
  const color =
    tone === "pass"
      ? "text-status-pass"
      : tone === "warn"
        ? "text-status-warn"
        : "text-status-merge";
  return (
    <span className={`chip ${color}`}>
      {tone === "warn" ? (
        <span className="dot dot-pulse bg-status-warn" />
      ) : (
        <Check className="w-3 h-3" />
      )}
      {label}
    </span>
  );
}

export default function Home() {
  return (
    <div className="min-h-screen font-sans bg-bg-page text-text-primary selection:bg-brand-purple/30 grain">

      {/* Nav */}
      <nav className="fixed top-0 w-full z-50 glass-nav">
        <div className="max-w-6xl mx-auto px-6 h-16 flex items-center justify-between">
          <Logo />
          <div className="hidden md:flex items-center gap-7 font-mono text-xs text-text-secondary">
            <a href="#pipeline" className="hover:text-white transition-colors">pipeline</a>
            <a href="#criteria" className="hover:text-white transition-colors">acceptance_criteria</a>
            <a href="#next" className="hover:text-white transition-colors">up_next</a>
            <a
              href="https://github.com/djinnos/djinn"
              className="flex items-center gap-2 px-3.5 py-1.5 rounded-full border border-border hover:border-text-muted text-text-primary transition-colors"
            >
              <Github className="w-3.5 h-3.5" />
              <span>github</span>
            </a>
          </div>
        </div>
      </nav>

      <main className="relative">

        {/* ——— Hero: the proposal header ——— */}
        <section className="relative px-6 pt-32 pb-24 md:pt-44 overflow-hidden">
          <div className="absolute inset-0 blueprint -z-10" />

          <div className="max-w-4xl mx-auto">
            {/* Proposal file header */}
            <div className="rise rise-1 font-mono text-xs text-text-muted flex flex-wrap items-center gap-x-3 gap-y-2 mb-10">
              <span className="text-text-secondary">proposal</span>
              <span className="text-brand-purple">#0001</span>
              <span aria-hidden>·</span>
              <span>targets: all_of_your_repos</span>
              <span aria-hidden>·</span>
              <span>
                status: <span className="text-status-pass">signed_off</span>
              </span>
            </div>

            <h1 className="rise rise-2 font-display text-5xl md:text-7xl lg:text-[5.25rem] font-semibold tracking-tight leading-[1.04] mb-8">
              From proposal<br />
              to <em className="stroke-under italic">pull request.</em>
            </h1>

            <p className="rise rise-3 text-lg md:text-xl text-text-secondary max-w-2xl leading-relaxed mb-10">
              Your team proposes and approves the work. AI agents build it,
              on your cluster, with your models, behind your review.
            </p>

            {/* Sign-off chips, markdown checkboxes inside */}
            <div className="rise rise-4 flex flex-wrap gap-2.5 mb-12">
              <span className="chip text-status-pass">
                <span className="select-none" aria-hidden>[x]</span> signed off · product
              </span>
              <span className="chip text-status-pass">
                <span className="select-none" aria-hidden>[x]</span> signed off · engineering
              </span>
              <span className="chip text-status-warn">
                <span className="select-none" aria-hidden>[ ]</span> building · djinn
                <span className="dot dot-pulse bg-status-warn" />
              </span>
            </div>

            <div className="rise rise-5 flex justify-start">
              <HeroActions />
            </div>
          </div>

          {/* Demo video as a window */}
          <div className="rise rise-5 mt-20 max-w-5xl mx-auto window">
            <div className="window-bar">
              <span className="dot bg-accent-coral/70" />
              <span className="dot bg-status-warn/70" />
              <span className="dot bg-status-pass/70" />
              <span className="ml-3">djinn — first look · demo</span>
            </div>
            <div className="aspect-video">
              <iframe
                src="https://www.youtube-nocookie.com/embed/cewtCRdkUuk"
                title="Djinn — first look demo"
                className="w-full h-full"
                allow="accelerometer; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share"
                allowFullScreen
              />
            </div>
          </div>
        </section>

        {/* ——— Pipeline ——— */}
        <section id="pipeline" className="px-6 py-28">
          <div className="max-w-4xl mx-auto">
            <div className="font-mono text-xs text-text-muted mb-3">## pipeline</div>
            <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-16">
              Every change takes the same road.
            </h2>

            <div className="relative">
              {/* rail */}
              <div className="rail absolute left-[19px] top-0 bottom-0 hidden sm:block" />

              <div className="space-y-14">
                {PIPELINE.map((stage) => (
                  <div key={stage.n} className="relative sm:pl-20">
                    <div className="rail-node hidden sm:flex absolute left-0 top-0 w-10 h-10 rounded-lg items-center justify-center text-xs text-brand-purple">
                      {stage.n}
                    </div>

                    <div className="flex flex-wrap items-baseline gap-x-4 gap-y-2 mb-3">
                      <h3 className="font-display text-2xl md:text-3xl font-semibold">
                        {stage.name}
                      </h3>
                      <StatusChip label={stage.status.label} tone={stage.status.tone} />
                    </div>

                    <p className="text-text-secondary leading-relaxed max-w-2xl">
                      {stage.body}
                    </p>

                    {stage.log && (
                      <div className="mt-6 max-w-2xl window">
                        <div className="window-bar">task-runs · namespace: djinn</div>
                        <div className="p-4 font-mono text-xs leading-relaxed overflow-x-auto">
                          {stage.log.map(([prefix, line], j) => (
                            <div key={j} className="whitespace-pre">
                              <span className="text-text-muted">{prefix} </span>
                              <span className={prefix === "$" ? "text-text-primary" : "text-text-secondary"}>
                                {line}
                              </span>
                            </div>
                          ))}
                        </div>
                      </div>
                    )}
                  </div>
                ))}
              </div>
            </div>
          </div>
        </section>

        {/* ——— Roadmap deep dive ——— */}
        <section className="px-6 py-20 border-y border-border-faint bg-bg-surface/50">
          <div className="max-w-6xl mx-auto grid md:grid-cols-5 gap-12 items-center">
            <div className="md:col-span-3 window">
              <div className="window-bar">fig. 01 — board · agents building in parallel across projects</div>
              <LightboxImage
                src="/kanban.jpg"
                alt="Djinn board — AI agents working tasks in parallel across multiple projects"
                className="w-full"
              />
            </div>
            <div className="md:col-span-2">
              <h3 className="font-display text-2xl md:text-3xl font-semibold mb-4">
                Approved specs become coordinated work
              </h3>
              <p className="text-text-secondary leading-relaxed">
                When a proposal graduates, djinn plans the epics, decomposes them
                into tasks with dependencies and blockers, and dispatches agents
                wave by wave, in parallel, across every project the spec touches.
                Change your mind mid-build? Freeze it, rework the spec, re-sign,
                and go again; the board reconciles.
              </p>
            </div>
          </div>
        </section>

        {/* ——— Acceptance criteria (features) ——— */}
        <section id="criteria" className="px-6 py-28">
          <div className="max-w-4xl mx-auto">
            <div className="font-mono text-xs text-text-muted mb-3">## acceptance_criteria</div>
            <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-4">
              What it takes to hand AI the keyboard.
            </h2>
            <p className="text-text-secondary text-lg mb-14 max-w-2xl">
              Six criteria, all met, before any of this is worth running on your infrastructure.
            </p>

            <div className="grid sm:grid-cols-2 gap-x-12 gap-y-10">
              {CRITERIA.map((c, i) => (
                <div key={i} className="flex gap-4">
                  <span className="font-mono text-status-pass text-sm pt-1 select-none whitespace-nowrap" aria-hidden>
                    - [x]
                  </span>
                  <div>
                    <h3 className="font-semibold text-lg mb-1.5">{c.title}</h3>
                    <p className="text-text-secondary text-[15px] leading-relaxed">{c.body}</p>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </section>

        {/* ——— Memory deep dive ——— */}
        <section className="px-6 py-20 border-y border-border-faint bg-bg-surface/50">
          <div className="max-w-6xl mx-auto grid md:grid-cols-5 gap-12 items-center">
            <div className="md:col-span-2 order-2 md:order-1">
              <h3 className="font-display text-2xl md:text-3xl font-semibold mb-4">
                Agents that know your codebase
              </h3>
              <p className="text-text-secondary leading-relaxed">
                A per-project code graph (8 languages) powers impact analysis and
                dead-code detection, and a knowledge base of linked notes carries
                decisions, patterns, and pitfalls from one task into the next. Your
                100th task is informed by everything the first 99 learned.
              </p>
            </div>
            <div className="md:col-span-3 order-1 md:order-2 window">
              <div className="window-bar">fig. 02 — code graph · symbols, references, dependencies</div>
              <LightboxImage
                src="/code-graph.jpg"
                alt="Djinn Code Graph — per-project symbol graph powering impact analysis and code intelligence"
                className="w-full"
              />
            </div>
          </div>
        </section>

        {/* ——— Up next: proposal #0002, status draft ——— */}
        <section id="next" className="px-6 py-28">
          <div className="max-w-4xl mx-auto">
            <div className="rounded-xl border border-dashed border-border p-8 md:p-12">
              <div className="font-mono text-xs text-text-muted flex flex-wrap items-center gap-x-3 gap-y-2 mb-8">
                <span className="text-text-secondary">proposal</span>
                <span className="text-brand-purple">#0002</span>
                <span aria-hidden>·</span>
                <span>
                  status: <span className="text-status-warn">draft</span>
                </span>
                <span aria-hidden>·</span>
                <span>feedback welcome</span>
              </div>

              <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-5">
                See where the tokens go.
              </h2>
              <p className="text-text-secondary text-lg leading-relaxed max-w-2xl mb-10">
                AI spend is opaque almost everywhere. Djinn&apos;s pipeline makes it
                attributable: every session belongs to a task, every task to an
                epic, every epic to a proposal, all under a real user. The
                visibility layer comes next.
              </p>

              <div className="space-y-5 font-mono text-sm">
                {[
                  ["spend_attribution", "tokens and cost per proposal, project, model, user, and role, not one blind number on an invoice."],
                  ["value_delivered", "proposals shipped, PRs merged, rework loops, review pass rates: cost next to outcome."],
                  ["tracing_built_in", "every agent session already streams to Langfuse; next, rolled up into answers a lead can act on."],
                ].map(([k, v], i) => (
                  <div key={i} className="flex flex-col sm:flex-row sm:items-baseline gap-1 sm:gap-4">
                    <span className="text-status-warn shrink-0 w-56 whitespace-nowrap">[ ] {k}</span>
                    <span className="text-text-secondary font-sans text-[15px]">{v}</span>
                  </div>
                ))}
              </div>
            </div>
          </div>
        </section>

        {/* ——— Merged: final CTA ——— */}
        <section className="px-6 pb-32">
          <div className="max-w-4xl mx-auto rounded-xl border border-status-merge/30 bg-status-merge/[0.06] p-10 md:p-14 text-center">
            <div className="inline-flex items-center gap-2 font-mono text-xs px-3 py-1.5 rounded-full bg-status-merge/15 text-status-merge border border-status-merge/30 mb-8">
              <GitMerge className="w-3.5 h-3.5" />
              merged
            </div>
            <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-4">
              Your backlog, merged.
            </h2>
            <p className="text-text-secondary text-lg mb-10 max-w-xl mx-auto">
              Source-available and free to self-host. One Helm chart. A single VPS
              with k3s is enough to start, and your code and credentials never
              leave your infrastructure.
            </p>
            <HeroActions />
          </div>
        </section>

        <Footer />
      </main>
    </div>
  );
}
