import {
  Cpu,
  Layers,
  Sparkles,
  Play,
  CheckCircle2,
  Github,
  Network,
  FolderGit2,
  Shuffle,
  FileText,
  Users,
  Server,
  BarChart3,
  Coins,
  GitPullRequest,
} from "lucide-react";
import HeroActions from "../components/HeroActions";
import Logo from "../components/Logo";
import Footer from "../components/Footer";
import LightboxImage from "../components/Lightbox";

export default function Home() {
  return (
    <div className="min-h-screen font-sans bg-bg-page text-text-primary selection:bg-brand-purple/30">

      {/* Nav */}
      <nav className="fixed top-0 w-full z-50 glass-nav">
        <div className="max-w-7xl mx-auto px-6 h-16 flex items-center justify-between">
          <Logo />
          <div className="hidden md:flex items-center gap-6 text-sm font-medium text-text-secondary">
            <a href="#features" className="hover:text-white transition-colors">Features</a>
            <a href="#how-it-works" className="hover:text-white transition-colors">How it Works</a>
            <a href="#visibility" className="hover:text-white transition-colors">Visibility</a>
            <a href="https://github.com/djinnos/djinn" className="flex items-center gap-2 px-4 py-2 rounded-full bg-white/5 hover:bg-white/10 border border-white/5 transition-all text-white">
              <Github className="w-4 h-4" />
              <span>GitHub</span>
            </a>
          </div>
        </div>
      </nav>

      <main>

        {/* 1. Hero Section */}
        <section className="relative px-6 pt-32 pb-32 md:pt-48 md:pb-48 hero-gradient overflow-hidden">
          {/* Decorative Elements */}
          <div className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-[800px] h-[800px] bg-brand-purple/10 rounded-full blur-[100px] -z-10 pointer-events-none" />

          <div className="max-w-5xl mx-auto text-center relative z-10">
            <div className="inline-flex items-center gap-2 px-3 py-1 rounded-full bg-brand-purple/10 border border-brand-purple/20 text-brand-purple text-xs font-medium mb-8 animate-float">
              <Sparkles className="w-3 h-3" />
              <span>Open Source · Kubernetes-Native</span>
            </div>

            <h1 className="text-5xl md:text-7xl lg:text-8xl font-bold tracking-tight mb-8 leading-[1.1]">
              From proposal to <span className="text-transparent bg-clip-text bg-gradient-to-r from-brand-purple to-purple-400">pull request.</span>
            </h1>

            <p className="text-xl md:text-2xl text-text-secondary max-w-3xl mx-auto leading-relaxed mb-12 font-light">
              Your team proposes and approves the work. AI agents build it — on your cluster, with your models, behind your review.
            </p>

            <HeroActions />

          </div>

          {/* App Preview */}
          <div className="mt-20 max-w-5xl mx-auto relative group">
             <div className="absolute -inset-1 bg-gradient-to-b from-brand-purple/20 to-transparent rounded-2xl blur-lg opacity-50 group-hover:opacity-75 transition-opacity" />
             <div className="relative bg-[#1a1a1a] rounded-xl border border-border p-2 shadow-2xl">
               <LightboxImage
                 src="/kanban.jpg"
                 alt="Djinn — parallel AI agents building approved work across multiple projects"
                 className="rounded-lg w-full"
               />
             </div>
          </div>
        </section>

        {/* 2. How It Works */}
        <section id="how-it-works" className="pt-0 pb-32 px-6">
          <div className="max-w-7xl mx-auto">
            <div className="text-center mb-16">
              <h2 className="text-3xl md:text-5xl font-bold mb-6">How It Works</h2>
              <p className="text-text-secondary text-lg max-w-2xl mx-auto">Specs, not prompts. Every change starts as a reviewed proposal and ends as a pull request you merge.</p>
            </div>

            <div className="grid md:grid-cols-4 gap-12 relative">
              {/* Connector Line (Desktop) */}
              <div className="hidden md:block absolute top-12 left-[12%] right-[12%] h-0.5 bg-gradient-to-r from-bg-surface-elevated via-brand-purple/50 to-bg-surface-elevated -z-10" />

              {[
                {
                  step: "01",
                  title: "Propose",
                  desc: "Anyone writes a proposal — a problem, a goal, acceptance criteria. A living spec that can target one repo or many. Djinn helps draft and refine it.",
                  icon: <FileText className="w-16 h-16 text-accent-peach" />
                },
                {
                  step: "02",
                  title: "Review & Sign Off",
                  desc: "Product and engineering leave feedback; the spec evolves revision by revision. Sign-offs go stale if it changes after — approval always means this version.",
                  icon: <Users className="w-16 h-16 text-accent-mint" />
                },
                {
                  step: "03",
                  title: "Djinn Builds",
                  desc: "Graduation turns the spec into epics and tasks. Agents work in parallel, each in its own isolated Kubernetes Job, using the models you configured.",
                  icon: <Play className="w-16 h-16 text-brand-purple ml-2" />
                },
                {
                  step: "04",
                  title: "You Merge",
                  desc: "An AI reviewer checks every result against the acceptance criteria; rejected work loops back. What passes becomes a pull request. Nothing ships without you.",
                  icon: <CheckCircle2 className="w-16 h-16 text-accent-mint" />
                }
              ].map((item, i) => (
                <div key={i} className="relative flex flex-col items-center text-center">
                   <div className="flex items-center justify-center mb-8 relative z-10 bg-bg-page p-4">
                     {item.icon}
                   </div>
                   <div className="space-y-4 max-w-sm">
                     <div className="text-sm font-bold tracking-widest text-text-muted uppercase">Step {item.step}</div>
                     <h3 className="text-2xl font-bold text-white">{item.title}</h3>
                     <p className="text-text-secondary leading-relaxed">{item.desc}</p>
                   </div>
                </div>
              ))}
            </div>
          </div>
        </section>

        {/* 3. Feature Grid */}
        <section id="features" className="py-32 px-6 bg-bg-surface border-y border-border">
          <div className="max-w-7xl mx-auto">
             <div className="grid md:grid-cols-3 gap-8">
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-brand-purple/50 transition-colors">
                 <FileText className="w-10 h-10 text-brand-purple mb-6" />
                 <h3 className="text-xl font-bold mb-2">Proposal-Driven</h3>
                 <div className="text-sm font-mono text-brand-purple mb-4">&quot;Argue before the tokens burn&quot;</div>
                 <p className="text-text-secondary">Work starts as a written spec with acceptance criteria — feedback threads, revisions, and stale-aware sign-offs. The team aligns on what to build before any agent runs.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-mint/50 transition-colors">
                 <Server className="w-10 h-10 text-accent-mint mb-6" />
                 <h3 className="text-xl font-bold mb-2">Your Cloud</h3>
                 <div className="text-sm font-mono text-accent-mint mb-4">&quot;Your cluster, your rules&quot;</div>
                 <p className="text-text-secondary">Kubernetes-native and self-hosted: one Helm chart deploys on anything from a one-box k3s VPS to EKS, GKE, or AKS. Your code and credentials never leave your infrastructure.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-peach/50 transition-colors">
                 <Shuffle className="w-10 h-10 text-accent-peach mb-6" />
                 <h3 className="text-xl font-bold mb-2">Your Models</h3>
                 <div className="text-sm font-mono text-accent-peach mb-4">&quot;Use what you already pay for&quot;</div>
                 <p className="text-text-secondary">Anthropic, OpenAI, Google, Bedrock, Vertex, Azure, Copilot, Codex, any OpenAI-compatible endpoint. One model for coding, another for review — per user, per project, per role.</p>
               </div>
             </div>

             <div className="grid md:grid-cols-3 gap-8 mt-8">
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-brand-purple/30 transition-colors">
                 <FolderGit2 className="w-10 h-10 text-brand-purple mb-6" />
                 <h3 className="text-xl font-bold mb-2">Multi-Project, Multi-User</h3>
                 <div className="text-sm font-mono text-brand-purple mb-4">&quot;One board for the whole team&quot;</div>
                 <p className="text-text-secondary">Every repo gets its own devcontainer image, code graph, and knowledge base. Every teammate brings their own credentials and limits. One proposal can drive changes across many repos.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-mint/30 transition-colors">
                 <Cpu className="w-10 h-10 text-accent-mint mb-6" />
                 <h3 className="text-xl font-bold mb-2">Parallel by Default</h3>
                 <div className="text-sm font-mono text-accent-mint mb-4">&quot;Scale by adding nodes&quot;</div>
                 <p className="text-text-secondary">Each task runs as an isolated Kubernetes Job in its own workspace. The coordinator dispatches by priority and dependency order, capped per user and per model.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-peach/30 transition-colors">
                 <GitPullRequest className="w-10 h-10 text-accent-peach mb-6" />
                 <h3 className="text-xl font-bold mb-2">Built-In Review</h3>
                 <div className="text-sm font-mono text-accent-peach mb-4">&quot;Nothing ships without you&quot;</div>
                 <p className="text-text-secondary">AI reviewers judge every change against the proposal&apos;s acceptance criteria and send weak work back. You get a clean pull request — and the final say.</p>
               </div>
             </div>
          </div>
        </section>

        {/* 4. Feature Deep Dives */}
        <section className="py-32 px-6 space-y-32">
          {/* Block A */}
          <div className="max-w-6xl mx-auto grid md:grid-cols-2 gap-16 items-center">
            <div className="order-2 md:order-1 relative group">
              <div className="absolute -inset-1 bg-gradient-to-tr from-brand-purple to-accent-peach rounded-2xl blur opacity-20 group-hover:opacity-40 transition-duration-500" />
              <div className="relative rounded-xl border border-border bg-bg-surface overflow-hidden">
                <LightboxImage
                  src="/epics.jpg"
                  alt="Djinn Roadmap — epics and tasks generated from an approved proposal"
                  className="w-full"
                />
              </div>
            </div>
            <div className="order-1 md:order-2">
              <div className="w-12 h-12 rounded-full bg-brand-purple/10 flex items-center justify-center mb-6">
                <Layers className="w-6 h-6 text-brand-purple" />
              </div>
              <h3 className="text-3xl font-bold mb-4">Approved Specs Become Coordinated Work</h3>
              <p className="text-text-secondary text-lg leading-relaxed">
                When a proposal graduates, Djinn plans the epic, decomposes it into tasks with dependencies and blockers, and dispatches agents wave by wave. Change your mind mid-build? Freeze it, rework the spec, re-sign, and go again — the board reconciles.
              </p>
            </div>
          </div>

          {/* Block B */}
          <div className="max-w-6xl mx-auto grid md:grid-cols-2 gap-16 items-center">
            <div>
              <div className="w-12 h-12 rounded-full bg-accent-mint/10 flex items-center justify-center mb-6">
                <Network className="w-6 h-6 text-accent-mint" />
              </div>
              <h3 className="text-3xl font-bold mb-4">Agents That Know Your Codebase</h3>
              <p className="text-text-secondary text-lg leading-relaxed">
                A per-project code graph (8 languages) powers impact analysis and dead-code detection, and a DB-backed knowledge base of linked notes carries decisions, patterns, and pitfalls from one task into the next. Your 100th task is informed by everything the first 99 learned.
              </p>
            </div>
            <div className="relative group">
              <div className="absolute -inset-1 bg-gradient-to-tr from-accent-mint to-blue-500 rounded-2xl blur opacity-20 group-hover:opacity-40 transition-duration-500" />
              <div className="relative rounded-xl border border-border bg-bg-surface overflow-hidden">
                <LightboxImage
                  src="/memory.jpg"
                  alt="Djinn Memory Graph — knowledge base visualization with connected decisions, patterns, and architecture notes"
                  className="w-full"
                />
              </div>
            </div>
          </div>
        </section>

        {/* 5. Visibility (What's Next) */}
        <section id="visibility" className="py-32 px-6 bg-bg-surface border-y border-border">
          <div className="max-w-7xl mx-auto">
            <div className="text-center mb-16">
              <div className="inline-flex items-center gap-2 px-3 py-1 rounded-full bg-accent-peach/10 border border-accent-peach/20 text-accent-peach text-xs font-medium mb-6">
                <Sparkles className="w-3 h-3" />
                <span>Up Next</span>
              </div>
              <h2 className="text-3xl md:text-5xl font-bold mb-6">See Where the Tokens Go</h2>
              <p className="text-text-secondary text-lg max-w-3xl mx-auto">
                AI spend is opaque almost everywhere. Djinn&apos;s pipeline makes it attributable: every session belongs to a task, every task to an epic, every epic to a proposal — under a real user. We&apos;re building the visibility layer on top.
              </p>
            </div>

            <div className="grid md:grid-cols-3 gap-8">
              <div className="p-8 rounded-2xl bg-bg-page border border-border">
                <Coins className="w-10 h-10 text-accent-peach mb-6" />
                <h3 className="text-xl font-bold mb-2">Spend Attribution</h3>
                <p className="text-text-secondary">Tokens and cost per proposal, per project, per model, per user, per role — not one blind number on a provider invoice.</p>
              </div>
              <div className="p-8 rounded-2xl bg-bg-page border border-border">
                <BarChart3 className="w-10 h-10 text-brand-purple mb-6" />
                <h3 className="text-xl font-bold mb-2">Value Delivered</h3>
                <p className="text-text-secondary">Proposals shipped, PRs merged, rework loops, review pass rates — cost next to outcome, where the decision gets made.</p>
              </div>
              <div className="p-8 rounded-2xl bg-bg-page border border-border">
                <Network className="w-10 h-10 text-accent-mint mb-6" />
                <h3 className="text-xl font-bold mb-2">Tracing Built In</h3>
                <p className="text-text-secondary">Every agent session already streams to Langfuse. Next: rolling those traces up into answers a team lead can act on.</p>
              </div>
            </div>
          </div>
        </section>

        {/* 6. Get Started */}
        <section className="py-32 px-6 text-center">
          <div className="max-w-3xl mx-auto bg-gradient-to-b from-brand-purple/10 to-transparent p-12 rounded-[3rem] border border-brand-purple/20">
            <h2 className="text-4xl font-bold mb-8">Run It on Your Cluster</h2>
            <p className="text-lg text-text-secondary mb-12">Open source. One Helm chart. Bring your own models — your code and credentials stay on your infrastructure.</p>

            <HeroActions />
          </div>
        </section>

        {/* 7. Footer */}
        <Footer />

      </main>
    </div>
  );
}
