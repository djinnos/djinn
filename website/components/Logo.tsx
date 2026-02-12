"use client";

import Image from "next/image";
import Link from "next/link";

export default function Logo() {
  return (
    <Link 
      href="/" 
      className="flex items-center gap-3 cursor-pointer hover:opacity-80 transition-opacity" 
      onClick={() => window.scrollTo({ top: 0, behavior: 'smooth' })}
    >
      <div className="relative w-8 h-8 rounded-md overflow-hidden border border-white/10 shadow-sm">
        <Image 
          src="/logo.png" 
          alt="Djinn Logo" 
          fill 
          className="object-contain"
          priority
        />
      </div>
      <span className="text-xl font-bold tracking-tight text-white">Djinn</span>
    </Link>
  );
}
