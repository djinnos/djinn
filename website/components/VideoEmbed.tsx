"use client";

import { useState } from "react";

interface VideoEmbedProps {
  id: string;
  title: string;
  poster: string;
  className?: string;
}

/* The player only loads on click. Until then it's our own poster and play
   button, so YouTube's title bar and branding never sit over the artwork —
   no embed parameter can hide those. */
export default function VideoEmbed({ id, title, poster, className }: VideoEmbedProps) {
  const [playing, setPlaying] = useState(false);

  if (playing) {
    return (
      <iframe
        src={`https://www.youtube-nocookie.com/embed/${id}?autoplay=1&rel=0&modestbranding=1`}
        title={title}
        className={`${className ?? ""} w-full h-full`}
        allow="autoplay; accelerometer; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
        allowFullScreen
      />
    );
  }

  return (
    <button
      type="button"
      onClick={() => setPlaying(true)}
      aria-label={`Play: ${title}`}
      className={`${className ?? ""} group relative w-full h-full block cursor-pointer`}
    >
      <img src={poster} alt="" aria-hidden className="w-full h-full object-cover" />
      <span className="absolute inset-0 flex items-center justify-center">
        <span className="flex items-center justify-center w-[68px] h-[48px] rounded-xl bg-bg-page/70 border border-white/15 backdrop-blur-sm transition-colors group-hover:bg-brand-purple-dark/90">
          <svg viewBox="0 0 24 24" className="w-6 h-6 translate-x-[1px] fill-text-primary" aria-hidden>
            <path d="M8 5v14l11-7z" />
          </svg>
        </span>
      </span>
    </button>
  );
}
