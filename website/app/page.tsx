import { Github, GitMerge } from "lucide-react";
import HeroActions from "../components/HeroActions";
import Logo from "../components/Logo";
import Footer from "../components/Footer";
import LightboxImage from "../components/Lightbox";
import FlameWrap from "../components/FlameWrap";
import VideoEmbed from "../components/VideoEmbed";
import TaskRunsTerminal from "../components/TaskRunsTerminal";
import Parallax from "../components/Parallax";
import CipherReveal from "../components/CipherReveal";

/* ————————————————————————————————————————————————————————————
   The page is styled as a proposal moving through djinn's own
   pipeline: header → review & sign-off → build log → merged.
   Mono = system chrome. Serif = the human voice.
———————————————————————————————————————————————————————————— */

/* One line each. The titles carry the message; long bodies turned this
   section into a wall of text. */
const CRITERIA = [
  {
    title: "Specs, not prompts",
    body: "Feedback threads, revision history, and sign-offs that go stale when the spec changes.",
  },
  {
    title: "Your cloud",
    body: (
      <>Any Kubernetes. One <code className="tick">Helm</code> chart, or a single VPS running <code className="tick">k3s</code>.</>
    ),
  },
  {
    title: "Your models",
    body: "Anthropic, OpenAI, Google, Bedrock, Vertex, Azure, or any OpenAI-compatible endpoint.",
  },
  {
    title: "Parallel by default",
    body: (
      <>Every task is an isolated <code className="tick">Kubernetes Job</code> in its own <code className="tick">git</code> workspace.</>
    ),
  },
  {
    title: "Multi-project, multi-user",
    body: "Per-repo devcontainers and code graphs. Per-teammate credentials and limits.",
  },
  {
    title: "Review built in",
    body: "AI reviewers check every change against the criteria. You get the final say.",
  },
];

export default function Home() {
  return (
    <div className="min-h-screen font-sans bg-bg-page text-text-primary selection:bg-brand-purple/30 grain">

      {/* Nav */}
      <nav className="fixed top-0 w-full z-50 glass-nav px-6">
        {/* px-6 lives on the nav, not this box, so the 6xl track matches the
            sections below and the logo shares their left edge */}
        <div className="max-w-6xl mx-auto h-16 flex items-center justify-between">
          <Logo />
          <div className="hidden md:flex items-center gap-7 font-mono text-xs text-text-secondary">
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
        <section className="relative px-6 pt-24 pb-24 md:pt-28 overflow-hidden">
          <div className="absolute inset-0 blueprint -z-10" />

          {/* z-10 keeps the copy above the flames rising off the video below */}
          {/* max-w-6xl matches the video below and the figure sections, so
              everything on the page shares one left edge */}
          <div className="hero-copy relative z-10 max-w-6xl mx-auto flex flex-col justify-center">
            {/* Page-coloured scrim: knocks the flames back behind the copy so
                it stays readable. Full-bleed and faded at the bottom so it
                never reads as a panel edge. Dropped from lg up, where the copy
                column is tall enough that the flames never reach the text. */}
            <div
              aria-hidden
              className="lg:hidden pointer-events-none absolute -z-10 top-0 -bottom-[170px] left-1/2 -translate-x-1/2 w-screen bg-bg-page/80 backdrop-blur-[3px] [mask-image:linear-gradient(to_bottom,transparent_0%,#000_14%,#000_78%,transparent_100%)]"
            />

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

            {/* Fluid size: fills the 6xl track on one line down to ~md, wraps
                naturally below that. The vw term is what keeps it filling. */}
            <h1 className="rise rise-2 font-display text-[clamp(2.75rem,6.6vw,5rem)] font-semibold tracking-tight leading-[1.04] mb-8">
              From proposal to <em className="stroke-under italic">pull request.</em>
            </h1>

            <p className="rise rise-3 text-lg md:text-xl text-text-secondary max-w-4xl leading-relaxed mb-10">
              Your team proposes and approves the work. AI agents build it,
              on your cluster, with your models, behind your review.
            </p>

            <div className="rise rise-4 flex justify-start">
              <HeroActions />
            </div>
          </div>

          {/* Demo video, set alight in the deep brand purple (#7c3aed).
              from={0} so the half-cut framing is untouched at first paint;
              it only starts drifting once you scroll. */}
          <Parallax className="mt-20 max-w-6xl mx-auto" from={0} to={-90}>
          <FlameWrap
            className="flame-fade rise rise-5"
            color={[0.486, 0.227, 0.929]}
            radius={12}
            height={420}
            spread={12}
          >
            <div className="aspect-video rounded-xl overflow-hidden">
              <VideoEmbed
                id="cewtCRdkUuk"
                title="Djinn — first look demo"
                poster="/demo-poster.jpg"
              />
            </div>
          </FlameWrap>
          </Parallax>
        </section>

        {/* ——— Roadmap deep dive ——— */}
        <section className="px-6 py-24">
          <div className="max-w-6xl mx-auto grid md:grid-cols-5 gap-12 items-center">
            <Parallax className="md:col-span-3 bleed-left" from={70} to={-70}>
              <div className="window">
                <LightboxImage
                  src="/kanban.jpg"
                  alt="Djinn board — AI agents working tasks in parallel across multiple projects"
                  className="w-full"
                />
              </div>
            </Parallax>
            <Parallax className="md:col-span-2" from={-35} to={35}>
              <h3 className="font-display text-2xl md:text-3xl font-semibold mb-4">
                Approved specs become coordinated work
              </h3>
              <p className="text-text-secondary leading-relaxed">
                A graduated proposal becomes epics and tasks, dispatched wave by
                wave across every project it touches. Change your mind mid-build
                and the board reconciles.
              </p>

              {/* Same wave of work, as the cluster sees it */}
              <div className="mt-8">
                <TaskRunsTerminal />
              </div>
            </Parallax>
          </div>
        </section>

        {/* ——— Acceptance criteria (features) ——— */}
        <section id="criteria" className="px-6 py-28">
          <div className="max-w-6xl mx-auto">
            <div className="font-mono text-xs text-text-muted mb-3">## acceptance_criteria</div>
            <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-4">
              What it takes to hand AI the keyboard.
            </h2>
            <p className="text-text-secondary text-lg mb-14 max-w-2xl">
              Six criteria, all met, before any of this is worth running on your infrastructure.
            </p>

            <div className="grid sm:grid-cols-2 lg:grid-cols-3 gap-x-10 gap-y-12">
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
        <section className="px-6 py-24">
          <div className="max-w-6xl mx-auto grid md:grid-cols-5 gap-12 items-center">
            <Parallax className="md:col-span-2 order-2 md:order-1" from={-35} to={35}>
              <h3 className="font-display text-2xl md:text-3xl font-semibold mb-4">
                Agents that know your codebase
              </h3>
              <p className="text-text-secondary leading-relaxed">
                A per-project code graph (8 languages) powers impact analysis and
                dead-code detection, and a knowledge base of linked notes carries
                decisions, patterns, and pitfalls from one task into the next. Your
                100th task is informed by everything the first 99 learned.
              </p>
            </Parallax>
            <Parallax className="md:col-span-3 order-1 md:order-2 bleed-right" from={70} to={-70}>
              <div className="window">
                <LightboxImage
                  src="/code-graph.jpg"
                  alt="Djinn Code Graph — per-project symbol graph powering impact analysis and code intelligence"
                  className="w-full"
                />
              </div>
            </Parallax>
          </div>
        </section>

        {/* ——— Up next: proposal #0002, status draft ——— */}
        <section id="next" className="px-6 py-28">
          <div className="max-w-4xl mx-auto">
            {/* Ciphered until you sweep the cursor over it — the section is
                about visibility that does not exist yet. */}
            {/* No radius: it defaults to 95% of the card's half-diagonal. */}
            <CipherReveal className="rounded-xl border border-dashed border-border p-8 md:p-12">
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
            </CipherReveal>
          </div>
        </section>

        {/* ——— Merged: final CTA ——— */}
        <section className="px-6 pb-32">
          <div className="max-w-4xl mx-auto">
            {/* A side branch folding into the trunk, ending on the merge
                commit. No card: the section above is already a box, and the
                graph carries the "merged" idea better than a border does. */}
            <svg
              aria-hidden
              viewBox="0 0 80 96"
              className="w-20 h-24 text-status-merge"
              fill="none"
            >
              <path
                d="M14 0 V96"
                stroke="currentColor"
                strokeOpacity="0.3"
                strokeWidth="2"
              />
              <path
                d="M62 0 V28 C62 48 46 52 20 52"
                stroke="currentColor"
                strokeOpacity="0.55"
                strokeWidth="2"
                strokeLinecap="round"
              />
              <circle cx="14" cy="52" r="7" fill="var(--color-bg-page)" />
              <circle
                cx="14"
                cy="52"
                r="6"
                stroke="currentColor"
                strokeWidth="2.5"
              />
            </svg>

            <div className="inline-flex items-center gap-2 font-mono text-xs px-3 py-1.5 rounded-full bg-status-merge text-white mt-6 mb-7">
              <GitMerge className="w-3.5 h-3.5" />
              merged
            </div>

            <h2 className="font-display text-3xl md:text-5xl font-semibold tracking-tight mb-4">
              Your backlog, merged.
            </h2>
            <p className="text-text-secondary text-lg mb-10 max-w-2xl">
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
