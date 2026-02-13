import { 
  Shield, 
  Cpu, 
  Layers, 
  Sparkles, 
  GitBranch, 
  Play, 
  CheckCircle2, 
  Github, 
  Bot, 
  Network,
  FolderGit2,
  Shuffle
} from "lucide-react";
import HeroActions from "../components/HeroActions";
import Logo from "../components/Logo";
import Footer from "../components/Footer";

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
              <span>Public Beta Now Available</span>
            </div>
            
            <h1 className="text-5xl md:text-7xl lg:text-8xl font-bold tracking-tight mb-8 leading-[1.1]">
              Manage AI agents, <span className="text-transparent bg-clip-text bg-gradient-to-r from-brand-purple to-purple-400">not terminals.</span>
            </h1>
            
            <p className="text-xl md:text-2xl text-text-secondary max-w-2xl mx-auto leading-relaxed mb-12 font-light">
              Organize work across multiple projects as epics and tasks. Run AI agents in parallel on your machine — you review every decision before it merges.
            </p>
            
            <HeroActions />
            
          </div>

          {/* App Preview */}
          <div className="mt-20 max-w-5xl mx-auto relative group">
             <div className="absolute -inset-1 bg-gradient-to-b from-brand-purple/20 to-transparent rounded-2xl blur-lg opacity-50 group-hover:opacity-75 transition-opacity" />
             <div className="relative bg-[#1a1a1a] rounded-xl border border-border p-2 shadow-2xl">
               <img
                 src="/kanban.jpg"
                 alt="Djinn Desktop — Kanban board with parallel AI agents across multiple projects"
                 className="rounded-lg w-full"
               />
             </div>
          </div>
        </section>

        {/* 3. How It Works */}
        <section id="how-it-works" className="pt-0 pb-32 px-6">
          <div className="max-w-7xl mx-auto">
            <div className="text-center mb-16">
              <h2 className="text-3xl md:text-5xl font-bold mb-6">How It Works</h2>
              <p className="text-text-secondary text-lg max-w-2xl mx-auto">From backlog to pull request — you review, you merge.</p>
            </div>
            
            <div className="grid md:grid-cols-3 gap-12 relative">
              {/* Connector Line (Desktop) */}
              <div className="hidden md:block absolute top-12 left-[16%] right-[16%] h-0.5 bg-gradient-to-r from-bg-surface-elevated via-brand-purple/50 to-bg-surface-elevated -z-10" />

              {[
                {
                  step: "01",
                  title: "Create Tasks",
                  desc: "Organize features, bugs, and tech debt as epics. Use the kanban board or let AI decompose your brief.",
                  icon: <GitBranch className="w-16 h-16 text-accent-peach" />
                },
                {
                  step: "02",
                  title: "Hit Play",
                  desc: "Djinn spawns AI agents in isolated git worktrees. Each task gets its own sandbox. Dependencies respected.",
                  icon: <Play className="w-16 h-16 text-brand-purple ml-2" />
                },
                {
                  step: "03",
                  title: "Review & Merge",
                  desc: "AI reviewers check each task against your acceptance criteria. You review the finished work and decide when to merge.",
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

        {/* 4. Feature Grid */}
        <section id="features" className="py-32 px-6 bg-bg-surface border-y border-border">
          <div className="max-w-7xl mx-auto">
             <div className="grid md:grid-cols-3 gap-8">
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-brand-purple/50 transition-colors">
                 <Bot className="w-10 h-10 text-brand-purple mb-6" />
                 <h3 className="text-xl font-bold mb-2">Parallel Execution</h3>
                 <div className="text-sm font-mono text-brand-purple mb-4">"No more juggling terminals"</div>
                 <p className="text-text-secondary">Run multiple AI agents in parallel, each in its own git worktree. Manage tasks on a kanban board instead of switching between terminal windows.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-mint/50 transition-colors">
                 <Shield className="w-10 h-10 text-accent-mint mb-6" />
                 <h3 className="text-xl font-bold mb-2">Local-First</h3>
                 <div className="text-sm font-mono text-accent-mint mb-4">"Your code never leaves"</div>
                 <p className="text-text-secondary">Everything runs on your machine. No cloud. No data leaving your infrastructure. Enterprise-ready from day one.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-peach/50 transition-colors">
                 <Shuffle className="w-10 h-10 text-accent-peach mb-6" />
                 <h3 className="text-xl font-bold mb-2">Mix & Match Models</h3>
                 <div className="text-sm font-mono text-accent-peach mb-4">"Use all your models at once"</div>
                 <p className="text-text-secondary">Use Claude for coding, GPT for reviews, Gemini for research — all at the same time. Set which models handle which tasks and at what priority. No manual switching.</p>
               </div>
             </div>

             <div className="grid md:grid-cols-2 gap-8 mt-8">
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-brand-purple/30 transition-colors">
                 <FolderGit2 className="w-10 h-10 text-brand-purple mb-6" />
                 <h3 className="text-xl font-bold mb-2">Multi-Project</h3>
                 <div className="text-sm font-mono text-brand-purple mb-4">"All your repos, one dashboard"</div>
                 <p className="text-text-secondary">Microservices, monorepos, multiple projects — Djinn manages them all in parallel. Each repo has its own task database and knowledge base. One app to direct everything.</p>
               </div>
               <div className="p-8 rounded-2xl bg-bg-page border border-border group hover:border-accent-mint/30 transition-colors">
                 <Cpu className="w-10 h-10 text-accent-mint mb-6" />
                 <h3 className="text-xl font-bold mb-2">Any LLM Provider</h3>
                 <div className="text-sm font-mono text-accent-mint mb-4">"Use what you already pay for"</div>
                 <p className="text-text-secondary">Works with any provider supported by OpenCode — Claude, GPT, Gemini, and more. Use your existing subscription plans or API keys. No vendor lock-in.</p>
               </div>
             </div>
          </div>
        </section>

        {/* 6. Feature Deep Dives */}
        <section className="py-32 px-6 space-y-32">
          {/* Block A */}
          <div className="max-w-6xl mx-auto grid md:grid-cols-2 gap-16 items-center">
            <div className="order-2 md:order-1 relative group">
              <div className="absolute -inset-1 bg-gradient-to-tr from-brand-purple to-accent-peach rounded-2xl blur opacity-20 group-hover:opacity-40 transition-duration-500" />
              <div className="relative rounded-xl border border-border bg-bg-surface overflow-hidden">
                <img
                  src="/epics.jpg"
                  alt="Djinn Roadmap — Epic dependency graph showing task organization across projects"
                  className="w-full"
                />
              </div>
            </div>
            <div className="order-1 md:order-2">
              <div className="w-12 h-12 rounded-full bg-brand-purple/10 flex items-center justify-center mb-6">
                <Layers className="w-6 h-6 text-brand-purple" />
              </div>
              <h3 className="text-3xl font-bold mb-4">You Define the Work</h3>
              <p className="text-text-secondary text-lg leading-relaxed">
                Most agents are stateless — they forget everything when you close the chat. Djinn organizes work as Epics, Stories, and Tasks that persist across weeks. You set the priorities, define the dependencies, and control what gets built and when.
              </p>
            </div>
          </div>

          {/* Block B */}
          <div className="max-w-6xl mx-auto grid md:grid-cols-2 gap-16 items-center">
            <div>
              <div className="w-12 h-12 rounded-full bg-accent-mint/10 flex items-center justify-center mb-6">
                <Network className="w-6 h-6 text-accent-mint" />
              </div>
              <h3 className="text-3xl font-bold mb-4">You Control What the AI Knows</h3>
              <p className="text-text-secondary text-lg leading-relaxed">
                Decisions, patterns, and architectural rules live in a human-readable knowledge base — markdown files you can read, edit, and version control. You decide what context agents get. Your 100th task is informed by every decision you've documented.
              </p>
            </div>
            <div className="relative group">
              <div className="absolute -inset-1 bg-gradient-to-tr from-accent-mint to-blue-500 rounded-2xl blur opacity-20 group-hover:opacity-40 transition-duration-500" />
              <div className="relative rounded-xl border border-border bg-bg-surface p-4 aspect-[4/3] overflow-hidden flex items-center justify-center">
                <div className="text-text-muted font-mono text-sm">[Memory Graph Visual]</div>
              </div>
            </div>
          </div>
        </section>

        {/* 8. Download */}
        <section className="py-32 px-6 text-center">
          <div className="max-w-3xl mx-auto bg-gradient-to-b from-brand-purple/10 to-transparent p-12 rounded-[3rem] border border-brand-purple/20">
            <h2 className="text-4xl font-bold mb-8">Get Started</h2>
            <p className="text-lg text-text-secondary mb-12">Free during beta. Bring your own API keys. You stay in control.</p>
            
            <HeroActions />
            
            <div className="text-sm text-text-muted mt-8">
              Works with any LLM provider supported by OpenCode — use your existing plans or API keys.
            </div>
          </div>
        </section>

        {/* 9. Footer */}
        <Footer />
        
      </main>
    </div>
  );
}
