import { Github, ArrowRight } from "lucide-react";

export default function HeroActions() {
  return (
    <div className="flex flex-col items-center gap-5">
      <div className="flex flex-col sm:flex-row gap-3 justify-center items-center">
        <a
          href="https://github.com/djinnos/djinn#deploy-kubernetes--a-single-vps-counts"
          className="group px-7 py-3.5 bg-text-primary text-bg-page rounded-lg font-semibold text-base flex items-center gap-2.5 hover:bg-white transition-colors"
        >
          Deploy on your cluster
          <ArrowRight className="w-4 h-4 transition-transform group-hover:translate-x-0.5" />
        </a>
        <a
          href="https://github.com/djinnos/djinn"
          className="px-7 py-3.5 rounded-lg font-semibold text-base border border-border text-text-primary hover:border-text-muted hover:bg-white/[0.03] transition-colors flex items-center gap-2.5"
        >
          <Github className="w-4 h-4" />
          View on GitHub
        </a>
      </div>

      <div className="font-mono text-xs text-text-muted">
        free to self-host · runs on a single VPS (k3s) or EKS / GKE / AKS
      </div>
    </div>
  );
}
