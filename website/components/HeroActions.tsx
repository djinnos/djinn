"use client";

import { useState, useEffect, useRef } from "react";
import { Github, Apple, Monitor, Terminal, ChevronDown } from "lucide-react";

function MacDropdown({ primary }: { primary: boolean }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  if (primary) {
    return (
      <div ref={ref} className="relative">
        <button
          onClick={() => setOpen(!open)}
          className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)] cursor-pointer"
        >
          <Apple className="w-5 h-5" />
          Download for Mac
          <ChevronDown className={`w-4 h-4 transition-transform ${open ? "rotate-180" : ""}`} />
        </button>
        {open && (
          <div className="absolute top-full left-0 right-0 mt-2 bg-white rounded-xl border border-gray-200 shadow-xl overflow-hidden z-50">
            <a
              href="/api/download?platform=mac-arm64"
              className="block px-6 py-3 text-bg-page font-medium hover:bg-gray-100 transition-colors"
              onClick={() => setOpen(false)}
            >
              Apple Silicon
            </a>
            <div className="px-6 py-3 text-gray-400 font-medium cursor-not-allowed flex items-center justify-between border-t border-gray-100">
              Intel
              <span className="text-xs bg-gray-100 text-gray-400 px-2 py-0.5 rounded-full">Coming Soon</span>
            </div>
          </div>
        )}
      </div>
    );
  }

  return (
    <div ref={ref} className="relative">
      <button
        onClick={() => setOpen(!open)}
        className="hover:text-white cursor-pointer transition-colors text-sm text-text-secondary flex items-center gap-1"
      >
        Download for Mac
        <ChevronDown className={`w-3 h-3 transition-transform ${open ? "rotate-180" : ""}`} />
      </button>
      {open && (
        <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 bg-bg-surface-elevated rounded-xl border border-border shadow-xl overflow-hidden z-50 min-w-[180px]">
          <a
            href="/api/download?platform=mac-arm64"
            className="block px-4 py-2.5 text-white text-sm font-medium hover:bg-white/10 transition-colors"
            onClick={() => setOpen(false)}
          >
            Apple Silicon
          </a>
          <div className="px-4 py-2.5 text-text-muted text-sm font-medium cursor-not-allowed flex items-center justify-between border-t border-border">
            Intel
            <span className="text-[10px] bg-white/5 text-text-muted px-1.5 py-0.5 rounded-full">Soon</span>
          </div>
        </div>
      )}
    </div>
  );
}

export default function HeroActions() {
  const [os, setOs] = useState<"mac" | "windows" | "linux" | "unknown">("unknown");

  useEffect(() => {
    const platform = window.navigator.platform.toLowerCase();
    if (platform.includes("mac")) setOs("mac");
    else if (platform.includes("win")) setOs("windows");
    else if (platform.includes("linux")) setOs("linux");
  }, []);

  const downloadLinks = {
    windows: "/api/download?platform=windows",
    linux: "/api/download?platform=linux-appimage",
  };

  const MainButton = () => {
    if (os === "windows") {
      return (
        <a href={downloadLinks.windows} className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)]">
          <Monitor className="w-5 h-5" />
          Download for Windows
        </a>
      );
    }
    
    if (os === "linux") {
      return (
        <a href={downloadLinks.linux} className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)]">
          <Terminal className="w-5 h-5" />
          Download for Linux
        </a>
      );
    }

    // Default to Mac (also covers "unknown")
    return <MacDropdown primary />;
  };

  return (
    <div className="flex flex-col items-center gap-6">
      <div className="flex flex-col sm:flex-row gap-4 justify-center items-center">
        <MainButton />
        <a href="https://github.com/djinnos/djinn" className="px-8 py-4 bg-bg-surface-elevated text-white rounded-xl font-bold text-lg border border-border hover:bg-white/5 transition-all flex items-center gap-3">
          <Github className="w-5 h-5" />
          View on GitHub
        </a>
      </div>
      
      <div className="text-sm text-text-secondary flex gap-6 justify-center relative">
        {os !== "mac" && <MacDropdown primary={false} />}
        {os !== "windows" && (
          <a href={downloadLinks.windows} className="hover:text-white cursor-pointer transition-colors">
            Download for Windows
          </a>
        )}
        {os !== "linux" && (
          <a href={downloadLinks.linux} className="hover:text-white cursor-pointer transition-colors">
            Download for Linux
          </a>
        )}
      </div>
    </div>
  );
}
