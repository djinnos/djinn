"use client";

import { useEffect, useRef, useSyncExternalStore, type ReactNode } from "react";

const CHARSET =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789<>[]{}/\\|+=*#%$@&^~?!:;-_.";

const FINE_POINTER = "(hover: hover) and (pointer: fine)";
const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";

/* Hover-only effect: without a fine pointer the card would stay ciphered
   forever, so touch and reduced-motion get the plain card. */
function subscribeCapability(onChange: () => void) {
  const pointer = window.matchMedia(FINE_POINTER);
  const motion = window.matchMedia(REDUCED_MOTION);
  pointer.addEventListener("change", onChange);
  motion.addEventListener("change", onChange);
  return () => {
    pointer.removeEventListener("change", onChange);
    motion.removeEventListener("change", onChange);
  };
}

const readCapability = () =>
  window.matchMedia(FINE_POINTER).matches &&
  !window.matchMedia(REDUCED_MOTION).matches;

interface Word {
  el: HTMLSpanElement;
  text: string;
  /** Point in the 0→1 sweep at which this word settles. */
  at: number;
  shown: boolean;
}

/** Chance an unresolved word re-rolls per tick, at rest and mid-decrypt. */
const IDLE_CHURN = 0.16;
const ACTIVE_CHURN = 0.9;

const scrambleOf = (word: string) => {
  let out = "";
  for (let i = 0; i < word.length; i++) {
    out += CHARSET[(Math.random() * CHARSET.length) | 0];
  }
  return out;
};

/* Wraps every word in a fixed-width inline-block so swapping its characters
   cannot reflow the paragraph. */
function splitWords(content: HTMLElement): Word[] {
  const walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT);
  const texts: Text[] = [];
  let node: Node | null;
  while ((node = walker.nextNode())) {
    if (node.nodeValue?.trim()) texts.push(node as Text);
  }

  const words: Word[] = [];
  for (const text of texts) {
    const fragment = document.createDocumentFragment();
    for (const part of text.nodeValue!.split(/(\s+)/)) {
      if (!part) continue;
      if (/^\s+$/.test(part)) {
        fragment.appendChild(document.createTextNode(part));
        continue;
      }
      const span = document.createElement("span");
      span.textContent = part;
      fragment.appendChild(span);
      words.push({ el: span, text: part, at: 0, shown: false });
    }
    text.parentNode?.replaceChild(fragment, text);
  }

  // Measure everything first, then write — one layout pass, not one per word.
  const rects = words.map((word) => word.el.getBoundingClientRect());
  const last = Math.max(words.length - 1, 1);
  words.forEach((word, i) => {
    word.el.style.display = "inline-block";
    word.el.style.width = `${rects[i].width}px`;
    word.el.style.whiteSpace = "pre";
    word.el.textContent = scrambleOf(word.text);
    // Mostly reading order, with enough jitter that it reads as a wave rather
    // than a straight wipe down the card.
    word.at = (i / last) * 0.72 + Math.random() * 0.28;
  });
  return words;
}

interface CipherRevealProps {
  children: ReactNode;
  className?: string;
  /** Glyph mutations per second. */
  rate?: number;
  /** How fast the sweep travels; higher settles sooner. */
  speed?: number;
}

/* The card's own text is scrambled in place, so every word keeps its real
   colour, size, weight and position. Hovering anywhere runs a sweep that
   settles each word back to its real characters; leaving re-scrambles. */
export default function CipherReveal({
  children,
  className,
  rate = 24,
  speed = 0.13,
}: CipherRevealProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);

  const enabled = useSyncExternalStore(
    subscribeCapability,
    readCapability,
    () => false,
  );

  useEffect(() => {
    const host = hostRef.current;
    const content = contentRef.current;
    if (!enabled || !host || !content) return;

    const snapshot = content.innerHTML;
    let words = splitWords(content);

    // 0 = fully ciphered, 1 = fully readable. Eased toward its target so the
    // reveal reads as motion rather than a switch.
    let progress = 0;
    let target = 0;
    let timer = 0;
    let visible = false;

    const tick = () => {
      progress += (target - progress) * speed;
      if (Math.abs(target - progress) < 0.004) progress = target;
      const churn = target > 0 ? ACTIVE_CHURN : IDLE_CHURN;

      for (const word of words) {
        if (progress > word.at) {
          if (!word.shown) {
            word.el.textContent = word.text;
            word.shown = true;
          }
        } else {
          word.shown = false;
          if (Math.random() < churn) word.el.textContent = scrambleOf(word.text);
        }
      }
      timer = window.setTimeout(tick, 1000 / Math.max(rate, 1));
    };

    const play = () => {
      if (timer) return;
      timer = window.setTimeout(tick, 0);
    };
    const pause = () => {
      clearTimeout(timer);
      timer = 0;
    };

    // Nothing churns while the card is off screen.
    const seen = new IntersectionObserver(
      (entries) => {
        visible = entries[entries.length - 1]?.isIntersecting ?? false;
        if (visible) play();
        else pause();
      },
      { threshold: 0 },
    );
    seen.observe(host);

    const onEnter = () => {
      target = 1;
      if (visible) play();
    };
    const onLeave = () => {
      target = 0;
      if (visible) play();
    };

    const remeasure = () => {
      content.innerHTML = snapshot;
      words = splitWords(content);
    };
    const observer = new ResizeObserver(remeasure);
    observer.observe(host);

    host.addEventListener("pointerenter", onEnter);
    host.addEventListener("pointerleave", onLeave);

    return () => {
      pause();
      seen.disconnect();
      observer.disconnect();
      host.removeEventListener("pointerenter", onEnter);
      host.removeEventListener("pointerleave", onLeave);
      content.innerHTML = snapshot;
    };
  }, [enabled, rate, speed]);

  return (
    <div ref={hostRef} className={className}>
      {/* The visible copy gets scrambled, so the accessible text is a second,
          screen-reader-only copy that is never touched. */}
      <div ref={contentRef} aria-hidden={enabled || undefined}>
        {children}
      </div>
      {enabled && <div className="sr-only">{children}</div>}
    </div>
  );
}
