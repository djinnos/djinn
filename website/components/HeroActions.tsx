"use client";

import { useState, useEffect, useRef } from "react";
import { ChevronDown, Github, Command, Apple, Monitor, Terminal } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";

export default function HeroActions() {
  const [os, setOs] = useState<"mac" | "windows" | "linux" | "unknown">("unknown");
  const [isOpen, setIsOpen] = useState(false);
  const [isSmallMacOpen, setIsSmallMacOpen] = useState(false);
  
  const mainDropdownRef = useRef<HTMLDivElement>(null);
  const smallDropdownRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const platform = window.navigator.platform.toLowerCase();
    if (platform.includes("mac")) setOs("mac");
    else if (platform.includes("win")) setOs("windows");
    else if (platform.includes("linux")) setOs("linux");
  }, []);

  // Close dropdowns when clicking outside
  useEffect(() => {
    function handleClickOutside(event: MouseEvent) {
      if (mainDropdownRef.current && !mainDropdownRef.current.contains(event.target as Node)) {
        setIsOpen(false);
      }
      if (smallDropdownRef.current && !smallDropdownRef.current.contains(event.target as Node)) {
        setIsSmallMacOpen(false);
      }
    }

    document.addEventListener("mousedown", handleClickOutside);
    return () => {
      document.removeEventListener("mousedown", handleClickOutside);
    };
  }, []);

  const downloadLinks = {
    mac_silicon: "/api/download?platform=mac-arm64",
    mac_intel: "/api/download?platform=mac-x64",
    windows: "/api/download?platform=windows",
    linux: "/api/download?platform=linux-appimage",
  };

  const MacDropdownMenu = () => (
    <div className="absolute top-full left-0 right-0 mt-2 bg-bg-surface border border-border rounded-xl overflow-hidden shadow-xl z-50 min-w-[240px]">
      <a href={downloadLinks.mac_silicon} className="flex items-center gap-3 px-4 py-3 hover:bg-white/5 transition-colors text-left group">
        <div className="w-8 h-8 rounded-lg bg-bg-surface-elevated flex items-center justify-center border border-border group-hover:border-brand-purple/50">
          <span className="text-xs font-bold text-white">M1</span>
        </div>
        <div>
          <div className="text-sm font-bold text-white">Apple Silicon</div>
          <div className="text-xs text-text-secondary">M1/M2/M3 chips</div>
        </div>
      </a>
      <div className="h-px bg-border/50 mx-4" />
      <a href={downloadLinks.mac_intel} className="flex items-center gap-3 px-4 py-3 hover:bg-white/5 transition-colors text-left group">
        <div className="w-8 h-8 rounded-lg bg-bg-surface-elevated flex items-center justify-center border border-border group-hover:border-brand-purple/50">
          <span className="text-xs font-bold text-white">Intel</span>
        </div>
        <div>
          <div className="text-sm font-bold text-white">Intel Chip</div>
          <div className="text-xs text-text-secondary">Older macs</div>
        </div>
      </a>
    </div>
  );

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
      <div className="relative" ref={mainDropdownRef}>
        <button 
          onClick={() => setIsOpen(!isOpen)}
          className="px-8 py-4 bg-white text-bg-page rounded-xl font-bold text-lg flex items-center gap-3 hover:bg-gray-100 transition-all shadow-[0_0_20px_rgba(168,85,247,0.3)] hover:shadow-[0_0_30px_rgba(168,85,247,0.5)]"
        >
          <Apple className="w-5 h-5" />
          Download for Mac
          <ChevronDown className={`w-4 h-4 transition-transform ${isOpen ? "rotate-180" : ""}`} />
        </button>

        <AnimatePresence>
          {isOpen && (
            <motion.div 
              initial={{ opacity: 0, y: 10 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: 10 }}
              className="z-50"
            >
              <MacDropdownMenu />
            </motion.div>
          )}
        </AnimatePresence>
      </div>
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
          <div className="relative" ref={smallDropdownRef}>
             <button 
                onClick={() => setIsSmallMacOpen(!isSmallMacOpen)} 
                className="hover:text-white cursor-pointer transition-colors flex items-center gap-1"
             >
               Download for Mac <ChevronDown className="w-3 h-3" />
             </button>
             <AnimatePresence>
              {isSmallMacOpen && (
                <motion.div 
                  initial={{ opacity: 0, y: 10 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: 10 }}
                  className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 z-50 w-64"
                >
                   <MacDropdownMenu />
                </motion.div>
              )}
             </AnimatePresence>
          </div>
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
