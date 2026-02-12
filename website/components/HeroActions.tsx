"use client";

import { useState, useEffect } from "react";
import { Github, Apple, Monitor, Terminal } from "lucide-react";

export default function HeroActions() {
  const [os, setOs] = useState<"mac" | "windows" | "linux" | "unknown">("unknown");

  useEffect(() => {
    const platform = window.navigator.platform.toLowerCase();
    if (platform.includes("mac")) setOs("mac");
    else if (platform.includes("win")) setOs("windows");
    else if (platform.includes("linux")) setOs("linux");
  }, []);

  const downloadLinks = {
    mac: "/api/download?platform=mac-arm64",
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
    return (
      <a href={downloadLinks.mac} className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)]">
        <Apple className="w-5 h-5" />
        Download for Mac
      </a>
    );
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
        {os !== "mac" && (
          <a href={downloadLinks.mac} className="hover:text-white cursor-pointer transition-colors">
            Download for Mac
          </a>
        )}
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
