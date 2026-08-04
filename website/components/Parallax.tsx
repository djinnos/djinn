"use client";

import {
  motion,
  useReducedMotion,
  useScroll,
  useSpring,
  useTransform,
} from "framer-motion";
import { useRef, type ReactNode } from "react";

interface ParallaxProps {
  children: ReactNode;
  /** Offset in px when the element enters the viewport from below. */
  from?: number;
  /** Offset in px by the time it has scrolled past the top. */
  to?: number;
  /** Layout classes. These stay on the outer box so grid/flex parents still
      see a normal child; only the inner box is transformed. */
  className?: string;
}

/* Drift on scroll. The spring is the point: without it the element tracks
   scroll exactly and just looks offset, with it the element lags and settles,
   which is what reads as "loose". */
export default function Parallax({
  children,
  from = 60,
  to = -60,
  className,
}: ParallaxProps) {
  const ref = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion();

  const { scrollYProgress } = useScroll({
    target: ref,
    offset: ["start end", "end start"],
  });

  const y = useSpring(useTransform(scrollYProgress, [0, 1], [from, to]), {
    stiffness: 55,
    damping: 20,
    mass: 0.6,
  });

  return (
    <div ref={ref} className={className}>
      <motion.div style={reduced ? undefined : { y, willChange: "transform" }}>
        {children}
      </motion.div>
    </div>
  );
}
