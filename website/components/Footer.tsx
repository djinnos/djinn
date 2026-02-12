import Link from "next/link";

export default function Footer() {
  return (
    <footer className="py-12 px-6 border-t border-border bg-bg-page">
      <div className="max-w-7xl mx-auto flex flex-col md:flex-row justify-between items-center gap-8">
        <div className="text-xs text-text-muted order-2 md:order-1">
          © {new Date().getFullYear()} Djinn AI, Inc.
        </div>
        
        <div className="flex gap-8 text-sm text-text-secondary order-1 md:order-2">
          <a href="https://github.com/djinnos/djinn" className="hover:text-white transition-colors">GitHub</a>
          <Link href="/terms" className="hover:text-white transition-colors">Terms</Link>
          <Link href="/privacy" className="hover:text-white transition-colors">Privacy</Link>
        </div>
      </div>
    </footer>
  );
}
