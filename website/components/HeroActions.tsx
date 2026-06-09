import { Github, BookOpen } from "lucide-react";

export default function HeroActions() {
  return (
    <div className="flex flex-col items-center gap-6">
      <div className="flex flex-col sm:flex-row gap-4 justify-center items-center">
        <a
          href="https://github.com/djinnos/djinn#deploy-kubernetes"
          className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)]"
        >
          <BookOpen className="w-5 h-5" />
          Deploy on your cluster
        </a>
        <a
          href="https://github.com/djinnos/djinn"
          className="px-8 py-4 bg-bg-surface-elevated text-white rounded-xl font-bold text-lg border border-border hover:bg-white/5 transition-all flex items-center gap-3"
        >
          <Github className="w-5 h-5" />
          View on GitHub
        </a>
      </div>

      <div className="text-sm text-text-secondary">
        Open source — one Helm chart, any cluster: kind, k3s, EKS, GKE, AKS.
      </div>
    </div>
  );
}
