/**
 * GalaxyScene — the react-three-fiber internals of the galaxy view.
 *
 * One InstancedMesh for all nodes (LOD tessellation), one LineSegments
 * buffer for all edges (additive blending, same-cluster 0.25 /
 * cross-cluster 0.06 intensity), channel-dominance glow boost feeding a
 * luminance-thresholded Bloom, density compensation from galaxyColors.
 * Positions are precomputed (layoutGalaxy / server warm) — nothing here
 * simulates forces; buffers rebuild only when data or highlight changes.
 */

import { useEffect, useMemo, useRef, type ComponentRef } from "react";
import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { OrbitControls } from "@react-three/drei";
import { Bloom, EffectComposer } from "@react-three/postprocessing";
import * as THREE from "three";

import {
  bloomIntensityScale,
  edgeIntensityScale,
  edgeKindColor,
  groupColor,
  heatColor,
  HEAT_MUTED,
  nodeBoostScale,
  nodeGlowBoost,
  type Rgb,
} from "./galaxyColors";
import type { GalaxyColorMode, GalaxyData, GalaxyDisplay } from "./galaxyTypes";

/** At most this many labels render, picked by visual weight. */
const MAX_LABELS = 70;
/** Hover raycasts are throttled to this interval — a full-graph ray-sphere
 * sweep (~65k instances) costs a few ms, fine at ~14Hz, jank at 120Hz. */
const HOVER_THROTTLE_MS = 70;
const IDLE_ROTATE_DELAY_MS = 20_000;
const BASE_BLOOM_INTENSITY = 1.45;
/** Dim factor for non-highlighted nodes while a selection is active. */
const NODE_DIM = 0.15;

export interface FlyTarget {
  center: [number, number, number];
  distance: number;
  /** Bump to retrigger the animation even for an identical target. */
  key: number;
}

interface SceneProps {
  data: GalaxyData;
  colorMode: GalaxyColorMode;
  /** Node ids to spotlight; everything else dims. Null = no highlight. */
  highlight: Set<string> | null;
  display: GalaxyDisplay;
  showLabels: boolean;
  flyTarget: FlyTarget;
  onNodePointer: (index: number | null, clientX?: number, clientY?: number) => void;
  onNodeClick: (index: number | null) => void;
  /** Fired when the GPU drops the WebGL context (three keeps silently not
   * drawing — the owner must offer a remount). */
  onContextLost?: () => void;
}

function baseColorOf(data: GalaxyData, index: number, colorMode: GalaxyColorMode): Rgb {
  const node = data.nodes[index];
  if (colorMode === "heat") {
    return node.heatEligible ? heatColor(node.heat ?? 0) : HEAT_MUTED;
  }
  return groupColor(node.group);
}

// ── Nodes ───────────────────────────────────────────────────────────────────

function sphereDetail(count: number): [number, number, number] {
  if (count <= 8_000) return [1, 32, 24];
  if (count <= 25_000) return [1, 16, 12];
  return [1, 10, 7];
}

function GalaxyNodes({
  data,
  colorMode,
  highlight,
  boost,
  onNodePointer,
  onNodeClick,
}: {
  data: GalaxyData;
  colorMode: GalaxyColorMode;
  highlight: Set<string> | null;
  /** nodeBoostScale(count) × user nodeGlow — 1 = full glow, 0 = flat. */
  boost: number;
  onNodePointer: SceneProps["onNodePointer"];
  onNodeClick: SceneProps["onNodeClick"];
}) {
  const meshRef = useRef<THREE.InstancedMesh>(null);
  const { camera, gl } = useThree();
  const count = data.nodes.length;
  const detail = useMemo(() => sphereDetail(count), [count]);
  // Whole-repo clouds: shrink stars as density grows (gated: 1.0 at ≤20k)
  // so clumps read as star fields instead of merged white patches.
  const sizeDamp = 1 / Math.pow(Math.max(1, count / 20_000), 1 / 6);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;
    const tempObj = new THREE.Object3D();
    const color = new THREE.Color();
    const hasHighlight = highlight !== null && highlight.size > 0;

    for (let i = 0; i < count; i++) {
      const node = data.nodes[i];
      const isLit = !hasHighlight || highlight.has(node.id);

      const s = node.size * sizeDamp * (isLit ? 0.5 : 0.2);
      tempObj.position.set(node.x, node.y, node.z);
      tempObj.scale.set(s, s, s);
      tempObj.updateMatrix();
      mesh.setMatrixAt(i, tempObj.matrix);

      const [r, g, b] = baseColorOf(data, i, colorMode);
      if (!isLit) {
        color.setRGB(r * NODE_DIM, g * NODE_DIM, b * NODE_DIM);
      } else {
        // Boost above 1.0 so bloom picks up the excess as glow corona.
        const fullBoost = nodeGlowBoost(r, g, b);
        const applied = 1 + (fullBoost - 1) * boost;
        color.setRGB(r * applied, g * applied, b * applied);
      }
      mesh.setColorAt(i, color);
    }
    mesh.instanceMatrix.needsUpdate = true;
    if (mesh.instanceColor) mesh.instanceColor.needsUpdate = true;
    mesh.computeBoundingSphere();
  }, [data, colorMode, highlight, boost, count, sizeDamp]);

  // Manual picking instead of R3F event raycasting: R3F would ray-sweep
  // every instance on EVERY pointer event; throttling the sweep ourselves
  // keeps hover live at any graph size. Click picks on pointerup with a
  // small drag guard so OrbitControls orbits don't count as clicks.
  useEffect(() => {
    const canvas = gl.domElement;
    const raycaster = new THREE.Raycaster();
    const pointer = new THREE.Vector2();
    let lastHover = 0;
    let downX = 0;
    let downY = 0;

    const pick = (event: PointerEvent): number | null => {
      const mesh = meshRef.current;
      if (!mesh) return null;
      const rect = canvas.getBoundingClientRect();
      pointer.set(
        ((event.clientX - rect.left) / rect.width) * 2 - 1,
        -((event.clientY - rect.top) / rect.height) * 2 + 1,
      );
      raycaster.setFromCamera(pointer, camera);
      const hits = raycaster.intersectObject(mesh, false);
      return hits.length > 0 ? (hits[0].instanceId ?? null) : null;
    };

    const onMove = (event: PointerEvent) => {
      const now = performance.now();
      if (now - lastHover < HOVER_THROTTLE_MS) return;
      lastHover = now;
      const index = pick(event);
      if (index === null) onNodePointer(null);
      else onNodePointer(index, event.clientX, event.clientY);
    };
    const onLeave = () => onNodePointer(null);
    const onDown = (event: PointerEvent) => {
      downX = event.clientX;
      downY = event.clientY;
    };
    const onUp = (event: PointerEvent) => {
      if (event.button !== 0) return;
      const drag = Math.hypot(event.clientX - downX, event.clientY - downY);
      if (drag > 5) return; // orbit, not a click
      onNodeClick(pick(event));
    };

    canvas.addEventListener("pointermove", onMove);
    canvas.addEventListener("pointerleave", onLeave);
    canvas.addEventListener("pointerdown", onDown);
    canvas.addEventListener("pointerup", onUp);
    return () => {
      canvas.removeEventListener("pointermove", onMove);
      canvas.removeEventListener("pointerleave", onLeave);
      canvas.removeEventListener("pointerdown", onDown);
      canvas.removeEventListener("pointerup", onUp);
    };
  }, [camera, gl, onNodePointer, onNodeClick]);

  return (
    <instancedMesh
      key={count}
      ref={meshRef}
      args={[undefined, undefined, count]}
      frustumCulled={false}
    >
      <sphereGeometry args={detail} />
      <meshBasicMaterial toneMapped={false} />
    </instancedMesh>
  );
}

// ── Edges ───────────────────────────────────────────────────────────────────

function GalaxyEdges({
  data,
  highlight,
  brightness,
}: {
  data: GalaxyData;
  highlight: Set<string> | null;
  brightness: number;
}) {
  const { positions, colors } = useMemo(() => {
    const indexById = new Map<string, number>();
    data.nodes.forEach((node, i) => indexById.set(node.id, i));

    const densityScale = edgeIntensityScale(data.edges.length) * brightness;
    // 0 at ≤50k edges (approved look untouched), full strength at ≥250k.
    const hubCoefficient =
      0.06 * Math.min(1, Math.max(0, (data.edges.length - 50_000) / 200_000));
    const hasHighlight = highlight !== null && highlight.size > 0;
    const positions = new Float32Array(data.edges.length * 6);
    const colors = new Float32Array(data.edges.length * 6);
    let valid = 0;

    for (const edge of data.edges) {
      const si = indexById.get(edge.source);
      const ti = indexById.get(edge.target);
      if (si === undefined || ti === undefined) continue;
      const s = data.nodes[si];
      const t = data.nodes[ti];

      const sLit = !hasHighlight || highlight.has(s.id);
      const tLit = !hasHighlight || highlight.has(t.id);
      if (hasHighlight && !sLit && !tLit) continue;

      // Same-cluster edges glow; cross-cluster strands are individually
      // faint and only read in aggregate — that's what keeps hub fans from
      // becoming searchlights.
      const sameCluster = (s.group ?? "") === (t.group ?? "");
      let intensity = sameCluster ? 0.25 : 0.06;
      if (hasHighlight) {
        // A selection stays at full strength (never density-scaled).
        intensity = sLit && tLit ? 0.5 : 0.04 * densityScale;
      } else {
        intensity *= densityScale;
        // Whole-repo regime only (ramps in past 50k edges): thousands of
        // near-parallel edges into mega-hub files stack into white beams;
        // dim each edge by its larger endpoint degree so trunks read as
        // faint cones instead of searchlights.
        if (hubCoefficient > 0) {
          const maxDegree = Math.max(s.degree, t.degree);
          intensity /= 1 + hubCoefficient * Math.sqrt(maxDegree);
        }
      }

      const [r, g, b] = edgeKindColor(edge.kind);
      const off = valid * 6;
      positions[off] = s.x;
      positions[off + 1] = s.y;
      positions[off + 2] = s.z;
      positions[off + 3] = t.x;
      positions[off + 4] = t.y;
      positions[off + 5] = t.z;
      colors[off] = r * intensity;
      colors[off + 1] = g * intensity;
      colors[off + 2] = b * intensity;
      colors[off + 3] = r * intensity;
      colors[off + 4] = g * intensity;
      colors[off + 5] = b * intensity;
      valid++;
    }
    return {
      positions: positions.slice(0, valid * 6),
      colors: colors.slice(0, valid * 6),
    };
  }, [data, highlight, brightness]);

  return (
    <lineSegments key={positions.length} frustumCulled={false}>
      <bufferGeometry>
        <bufferAttribute attach="attributes-position" args={[positions, 3]} />
        <bufferAttribute attach="attributes-color" args={[colors, 3]} />
      </bufferGeometry>
      <lineBasicMaterial
        vertexColors
        transparent
        opacity={1.0}
        blending={THREE.AdditiveBlending}
        depthWrite={false}
        toneMapped={false}
      />
    </lineSegments>
  );
}

// ── Labels ──────────────────────────────────────────────────────────────────

function makeLabelTexture(text: string): { texture: THREE.CanvasTexture; aspect: number } {
  const truncated = text.length > 26 ? `${text.slice(0, 25)}…` : text;
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d")!;
  const font = "500 30px 'Geist Variable', ui-sans-serif, system-ui, sans-serif";
  ctx.font = font;
  const metrics = ctx.measureText(truncated);
  canvas.width = Math.ceil(metrics.width) + 16;
  canvas.height = 44;
  ctx.font = font;
  ctx.textBaseline = "middle";
  ctx.shadowColor = "rgba(0,0,0,0.9)";
  ctx.shadowBlur = 6;
  ctx.fillStyle = "rgba(224,234,255,0.92)";
  ctx.fillText(truncated, 8, canvas.height / 2);
  const texture = new THREE.CanvasTexture(canvas);
  texture.generateMipmaps = false;
  texture.minFilter = THREE.LinearFilter;
  return { texture, aspect: canvas.width / canvas.height };
}

function GalaxyLabels({
  data,
  highlight,
}: {
  data: GalaxyData;
  highlight: Set<string> | null;
}) {
  const labelled = useMemo(() => {
    const pool = highlight
      ? data.nodes.filter((n) => highlight.has(n.id))
      : data.nodes;
    return [...pool]
      .sort((a, b) => b.size * (b.degree + 1) - a.size * (a.degree + 1))
      .slice(0, MAX_LABELS);
  }, [data, highlight]);

  const sprites = useMemo(
    () =>
      labelled.map((node) => ({
        node,
        ...makeLabelTexture(node.label),
      })),
    [labelled],
  );

  useEffect(
    () => () => {
      for (const s of sprites) s.texture.dispose();
    },
    [sprites],
  );

  return (
    <group>
      {sprites.map(({ node, texture, aspect }) => {
        const h = Math.max(7, node.size * 1.2);
        return (
          <sprite
            key={node.id}
            position={[node.x, node.y + node.size * 0.5 + h * 0.9, node.z]}
            scale={[h * aspect, h, 1]}
          >
            <spriteMaterial
              map={texture}
              transparent
              depthWrite={false}
              toneMapped={false}
              opacity={0.95}
            />
          </sprite>
        );
      })}
    </group>
  );
}

// ── Context-loss sentinel ───────────────────────────────────────────────────
//
// A lost WebGL context is NOT a React error: three simply stops drawing
// and the scene goes silently blank (heavy graphs on weak GPUs are the
// usual trigger). Surface it so the owner can offer a remount.

function ContextLossSentinel({ onLost }: { onLost?: () => void }) {
  const gl = useThree((s) => s.gl);
  useEffect(() => {
    if (!onLost) return;
    const canvas = gl.domElement;
    const handle = (event: Event) => {
      event.preventDefault();
      onLost();
    };
    canvas.addEventListener("webglcontextlost", handle);
    return () => canvas.removeEventListener("webglcontextlost", handle);
  }, [gl, onLost]);
  return null;
}

// ── GL context janitor ──────────────────────────────────────────────────────
//
// Browsers cap live WebGL contexts (~16) and reclaim dead ones lazily, so
// canvas churn (Storybook story switches, HMR) can exhaust the pool and
// the next mount fails with "context could not be created". Force-release
// the context on unmount so churn never accumulates.

function ContextJanitor() {
  const gl = useThree((s) => s.gl);
  useEffect(() => {
    const canvas = gl.domElement;
    return () => {
      // Deferred + connectivity-checked: StrictMode (and HMR) unmount and
      // immediately remount the same tree — releasing synchronously would
      // kill the context of a canvas that's about to be reused. Only a
      // canvas that actually left the DOM gets its context reclaimed.
      setTimeout(() => {
        if (!canvas.isConnected) {
          gl.getContext()?.getExtension("WEBGL_lose_context")?.loseContext();
        }
      }, 250);
    };
  }, [gl]);
  return null;
}

// ── Camera rig: fly-to + idle auto-rotate ───────────────────────────────────

type OrbitControlsImpl = ComponentRef<typeof OrbitControls>;

function CameraRig({ flyTarget }: { flyTarget: FlyTarget }) {
  const controlsRef = useRef<OrbitControlsImpl>(null);
  const camera = useThree((s) => s.camera);
  const animating = useRef(false);
  const lastInteraction = useRef(0);
  const desiredCamera = useRef(new THREE.Vector3());
  const desiredTarget = useRef(new THREE.Vector3());

  useEffect(() => {
    const [cx, cy, cz] = flyTarget.center;
    desiredTarget.current.set(cx, cy, cz);
    // Approach along the current viewing direction so the fly-to feels like
    // travel, not teleport-and-reorient.
    const direction = camera.position
      .clone()
      .sub(controlsRef.current?.target ?? new THREE.Vector3())
      .normalize();
    if (direction.lengthSq() < 0.5) direction.set(0.35, 0.25, 1).normalize();
    desiredCamera.current
      .copy(desiredTarget.current)
      .addScaledVector(direction, flyTarget.distance);
    animating.current = true;
  }, [flyTarget, camera]);

  useFrame(() => {
    const controls = controlsRef.current;
    if (!controls) return;

    if (animating.current) {
      // Lerp BOTH the camera and the controls target — moving only the
      // camera lets OrbitControls re-center and snap back next frame.
      camera.position.lerp(desiredCamera.current, 0.075);
      controls.target.lerp(desiredTarget.current, 0.075);
      if (camera.position.distanceTo(desiredCamera.current) < 1) {
        animating.current = false;
      }
      controls.update();
    }

    controls.autoRotate =
      !animating.current &&
      performance.now() - lastInteraction.current > IDLE_ROTATE_DELAY_MS;
  });

  return (
    <OrbitControls
      ref={controlsRef}
      makeDefault
      enableDamping
      dampingFactor={0.08}
      rotateSpeed={0.55}
      zoomSpeed={1.4}
      minDistance={10}
      maxDistance={50_000}
      autoRotateSpeed={0.4}
      onStart={() => {
        animating.current = false;
        lastInteraction.current = performance.now();
      }}
      onEnd={() => {
        lastInteraction.current = performance.now();
      }}
    />
  );
}

// ── Scene root ──────────────────────────────────────────────────────────────

export function GalaxyScene({
  data,
  colorMode,
  highlight,
  display,
  showLabels,
  flyTarget,
  onNodePointer,
  onNodeClick,
  onContextLost,
}: SceneProps) {
  const nodeCount = data.nodes.length;
  const boost = nodeBoostScale(nodeCount) * display.nodeGlow;
  const bloom = BASE_BLOOM_INTENSITY * bloomIntensityScale(nodeCount) * display.bloom;

  return (
    <Canvas
      dpr={[1, 1.5]}
      gl={{ antialias: false, alpha: false, powerPreference: "high-performance" }}
      camera={{ fov: 50, near: 0.1, far: 100_000, position: [0, 0, flyTarget.distance] }}
    >
      <color attach="background" args={["#06090f"]} />
      <ContextJanitor />
      <ContextLossSentinel onLost={onContextLost} />

      <GalaxyNodes
        data={data}
        colorMode={colorMode}
        highlight={highlight}
        boost={boost}
        onNodePointer={onNodePointer}
        onNodeClick={onNodeClick}
      />
      <GalaxyEdges data={data} highlight={highlight} brightness={display.edgeBrightness} />
      {showLabels && <GalaxyLabels data={data} highlight={highlight} />}

      <CameraRig flyTarget={flyTarget} />
      <EffectComposer multisampling={0}>
        <Bloom
          luminanceThreshold={0.3}
          luminanceSmoothing={0.7}
          intensity={bloom}
          mipmapBlur
          radius={0.6}
        />
      </EffectComposer>
    </Canvas>
  );
}
