import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowRight01Icon,
  CheckmarkCircle04Icon,
  CubeIcon,
  Loading02Icon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import logoSvg from "@/assets/logo.svg";
import type { Project } from "@/api/server";
import {
  createImage,
  listImages,
  setProjectImage,
  type CatalogImage,
  type ImageBuildStatus,
} from "@/api/images";
import { fetchDevcontainerStatus, fetchProjectStack } from "@/api/devcontainer";
import {
  fetchEnvironmentConfig,
  resetEnvironmentConfig,
} from "@/api/environmentConfig";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from "@/components/ui/accordion";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { InlineError } from "@/components/InlineError";
import { OnboardingProgress } from "@/components/onboarding/OnboardingProgress";

import {
  catalogConfigFromStack,
  availableImageName,
  findReusableImage,
  recommendedImageName,
} from "./projectImageRecommendation";

const MAX_STACK_POLLS = 30;
const STACK_POLL_MS = 2_000;

type SetupPhase = "idle" | "saving" | "assigned";

/**
 * Required first-project environment step. A project cannot dispatch until it
 * references a catalog image, so this gate either assigns an existing shared
 * image or creates one from Djinn's detected repository stack.
 */
export function ProjectImageOnboarding({
  project,
  onFinished,
}: {
  project: Project;
  onFinished: () => void | Promise<void>;
}) {
  const queryClient = useQueryClient();
  const stackPolls = useRef(0);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const [selectedImageId, setSelectedImageId] = useState("");
  const [phase, setPhase] = useState<SetupPhase>("idle");
  const [assignedName, setAssignedName] = useState<string | null>(null);
  const [mutationError, setMutationError] = useState<string | null>(null);

  const imagesQuery = useQuery({
    queryKey: ["catalog-images"],
    queryFn: listImages,
  });
  const environmentQuery = useQuery({
    queryKey: ["project-environment", project.id],
    queryFn: () => fetchEnvironmentConfig(project.id),
  });
  const stackQuery = useQuery({
    queryKey: ["onboarding", "project-stack", project.id],
    queryFn: async () => {
      stackPolls.current += 1;
      return fetchProjectStack(project.id);
    },
    refetchInterval: (query) =>
      !query.state.data?.stack && stackPolls.current < MAX_STACK_POLLS
        ? STACK_POLL_MS
        : false,
  });

  const images = useMemo(() => imagesQuery.data ?? [], [imagesQuery.data]);
  const selectedImage = useMemo(
    () => images.find((image) => image.id === selectedImageId),
    [images, selectedImageId],
  );
  const recommendedConfig = useMemo(
    () =>
      stackQuery.data?.stack
        ? catalogConfigFromStack(stackQuery.data.stack)
        : null,
    [stackQuery.data?.stack],
  );
  const recommendationName = recommendedConfig
    ? recommendedImageName(recommendedConfig)
    : null;
  const isAssigned = phase === "assigned";

  // Each full-screen onboarding transition moves keyboard/screen-reader focus
  // to the new page title. The intermediate saving mutation is deliberately
  // excluded so clicking a CTA does not cause a surprising focus jump.
  useEffect(() => {
    headingRef.current?.focus();
  }, [isAssigned]);

  const ensureDetectedProjectConfig = async () => {
    const existing = environmentQuery.data ?? (await fetchEnvironmentConfig(project.id));
    if (existing.seeded) return;

    const reset = await resetEnvironmentConfig(project.id);
    if (!reset.ok || !reset.config) {
      throw new Error(reset.error ?? "Detected environment is not ready yet");
    }
    queryClient.setQueryData(["project-environment", project.id], {
      ...existing,
      config: reset.config,
      seeded: true,
    });
  };

  const assign = async (imageId: string, imageName: string) => {
    const result = await setProjectImage(project.id, imageId);
    if (!result.ok) {
      // `project_set_image` historically persisted the assignment before a
      // fallible build enqueue. Recover honestly from that partial-success
      // shape instead of creating another image on retry.
      const status = await fetchDevcontainerStatus(project.id).catch(() => null);
      if (status?.error || status?.needs_image !== false) {
        throw new Error(result.error ?? "Could not assign the catalog image");
      }
    }
    setAssignedName(imageName);
    setPhase("assigned");
  };

  const assignExisting = async () => {
    if (!selectedImage) return;
    setMutationError(null);
    setPhase("saving");
    try {
      // Assigning an existing shared image still requires the project's own
      // detected workspaces to be persisted. Otherwise the Advanced path can
      // make the project dispatchable while leaving its code-graph workspace
      // configuration empty.
      if (!stackQuery.data?.stack) {
        throw new Error("Wait for repository detection before assigning an image");
      }
      await ensureDetectedProjectConfig();
      await assign(selectedImage.id, selectedImage.name);
    } catch (error) {
      setMutationError(
        error instanceof Error ? error.message : "Could not assign the catalog image",
      );
      setPhase("idle");
    }
  };

  const createDetected = async () => {
    setMutationError(null);
    setPhase("saving");
    try {
      const stack = stackQuery.data?.stack;
      if (!stack) {
        throw new Error("Detected environment is not ready yet");
      }
      // Persist repository-specific workspaces on the project, but derive the
      // shared image independently from the narrow detected stack below.
      await ensureDetectedProjectConfig();
      const config = recommendedConfig ?? catalogConfigFromStack(stack);
      const preferredName = recommendationName ?? recommendedImageName(config);
      const projectSlug = `${project.github_owner}/${project.github_repo}`;
      let image: CatalogImage | undefined;
      let lastCreateError = "Could not create the detected image";

      // Re-read before every create attempt. This makes page reloads and
      // concurrent tabs converge on a config-identical image without ever
      // reusing a same-name image whose build inputs differ.
      for (let attempt = 0; attempt < 3 && !image; attempt += 1) {
        const currentImages = await listImages();
        image = findReusableImage(currentImages, config);
        if (image) break;

        const name = availableImageName(currentImages, preferredName, projectSlug);
        const description = `Auto-detected from ${projectSlug}`;
        const created = await createImage({ name, description, config });
        if (created.ok && created.id) {
          image = {
            id: created.id,
            name,
            description,
            status: "none",
            config,
            servicePresets: [],
          };
          break;
        }
        lastCreateError = created.error ?? lastCreateError;
      }
      if (!image) {
        throw new Error(lastCreateError);
      }

      await assign(image.id, image.name);
      await queryClient.invalidateQueries({ queryKey: ["catalog-images"] });
    } catch (error) {
      setMutationError(
        error instanceof Error ? error.message : "Could not prepare the detected image",
      );
      setPhase("idle");
    }
  };

  if (phase === "assigned") {
    return (
      <OnboardingShell complete>
        <div className="flex flex-col items-center gap-5 text-center">
          <div className="flex h-12 w-12 items-center justify-center rounded-full bg-primary/15">
            <HugeiconsIcon
              icon={CheckmarkCircle04Icon}
              size={26}
              className="text-primary"
            />
          </div>
          <div>
            <h1
              ref={headingRef}
              tabIndex={-1}
              className="text-xl font-semibold outline-none"
            >
              Environment assigned
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              {assignedName} is assigned to {project.name}. You can enter Djinn
              now. Djinn will finish any required build in the background, and
              agents become available when it is ready.
            </p>
          </div>
          <Button className="px-8" onClick={() => void onFinished()}>
            Enter Djinn
            <HugeiconsIcon icon={ArrowRight01Icon} size={15} />
          </Button>
        </div>
      </OnboardingShell>
    );
  }

  const stackError = stackQuery.data?.error ??
    (stackQuery.error instanceof Error ? stackQuery.error.message : null);
  const detectionTimedOut =
    !stackQuery.data?.stack && stackPolls.current >= MAX_STACK_POLLS;
  const canPrepareDetectedImage = Boolean(stackQuery.data?.stack);

  return (
    <OnboardingShell>
      <div className="flex flex-col gap-6">
        <div className="text-center">
          <h1
            ref={headingRef}
            tabIndex={-1}
            className="text-xl font-semibold outline-none"
          >
            Prepare the runtime environment
          </h1>
          <p className="mt-1 text-sm text-muted-foreground">
            Confirm the environment Djinn detected for {project.name}. An image
            must be assigned before agents can run.
          </p>
        </div>

        {mutationError && <InlineError message={mutationError} />}

        {imagesQuery.error && (
          <InlineError
            message={
              imagesQuery.error instanceof Error
                ? imagesQuery.error.message
                : "Failed to load the image catalog"
            }
            onRetry={() => void imagesQuery.refetch()}
          />
        )}

        <section className="rounded-xl border border-primary/40 bg-gradient-to-br from-primary/[0.07] to-transparent p-5">
          <div className="flex items-start gap-3">
            <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
              {stackQuery.data?.stack ? (
                <HugeiconsIcon icon={CheckmarkCircle04Icon} size={20} className="text-primary" />
              ) : (
                <HugeiconsIcon
                  icon={Loading02Icon}
                  size={20}
                  className="animate-spin text-primary"
                />
              )}
            </div>
            <div className="min-w-0 flex-1">
              <h3 className="text-sm font-semibold">
                Recommended for this repository
              </h3>
              {recommendationName ? (
                <div className="mt-3 rounded-lg border border-primary/20 bg-background/40 px-4 py-3">
                  <p className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                    Detected setup
                  </p>
                  <p className="mt-1 text-base font-semibold text-foreground">
                    {recommendationName}
                  </p>
                  <p className="mt-1 text-xs text-muted-foreground">
                    Djinn will reuse an identical shared image when possible, or
                    create one without copying project secrets or hooks.
                  </p>
                </div>
              ) : (
                <p className="mt-0.5 text-xs text-muted-foreground">
                  {detectionTimedOut
                    ? "Detection is taking longer than expected. Retry after the repository mirror finishes."
                    : "Cloning the repository and detecting languages, versions, and package managers…"}
                </p>
              )}
              {stackError && (
                <div className="mt-3">
                  <InlineError
                    message={stackError}
                    onRetry={() => {
                      stackPolls.current = 0;
                      void stackQuery.refetch();
                    }}
                  />
                </div>
              )}
              <Button
                className="mt-4 w-full"
                disabled={!canPrepareDetectedImage || phase === "saving"}
                onClick={() => void createDetected()}
              >
                {phase === "saving" ? (
                  <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                ) : (
                  <HugeiconsIcon icon={CubeIcon} size={16} />
                )}
                {phase === "saving"
                  ? "Preparing environment…"
                  : "Use recommended environment"}
              </Button>
              {detectionTimedOut && !stackError && (
                <Button
                  variant="ghost"
                  size="sm"
                  className="mt-2 w-full"
                  onClick={() => {
                    stackPolls.current = 0;
                    void stackQuery.refetch();
                  }}
                >
                  Retry detection
                </Button>
              )}
            </div>
          </div>
        </section>

        <Accordion>
          <AccordionItem
            value="existing-images"
            className="rounded-xl border bg-card/20 px-5"
          >
            <AccordionTrigger className="hover:no-underline">
              <div>
                <span className="block text-sm font-semibold">Advanced</span>
                <span className="mt-0.5 block text-xs font-normal text-muted-foreground">
                  Choose an existing catalog image instead.
                </span>
              </div>
            </AccordionTrigger>
            <AccordionContent className="pb-4">
              {images.length === 0 ? (
                <p className="rounded-lg border border-dashed px-4 py-3 text-xs text-muted-foreground">
                  No existing catalog images are available yet.
                </p>
              ) : (
                <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
                  <Select
                    value={selectedImageId || null}
                    onValueChange={(value) => {
                      if (typeof value === "string") setSelectedImageId(value);
                    }}
                    disabled={!canPrepareDetectedImage || phase === "saving"}
                  >
                    <SelectTrigger
                      className="w-full flex-1 sm:min-w-64"
                      aria-label="Existing catalog image"
                    >
                      <SelectValue placeholder="Select a catalog image" />
                    </SelectTrigger>
                    <SelectContent>
                      {images.map((image) => (
                        <SelectItem
                          key={image.id}
                          value={image.id}
                          disabled={image.status === "failed"}
                        >
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-3">
                            <span className="truncate">{image.name}</span>
                            <ImageStatusBadge status={image.status} />
                          </span>
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <Button
                    variant="outline"
                    disabled={
                      !selectedImage ||
                      selectedImage.status === "failed" ||
                      !canPrepareDetectedImage ||
                      phase === "saving"
                    }
                    onClick={() => void assignExisting()}
                  >
                    Use image
                  </Button>
                </div>
              )}
              <p className="mt-3 text-xs text-muted-foreground">
                {canPrepareDetectedImage
                  ? "Ready and building images can be assigned. Failed images must be repaired from the Images page first."
                  : "Repository detection must finish before an image can be assigned, so Djinn can preserve the detected project workspaces."}
              </p>
            </AccordionContent>
          </AccordionItem>
        </Accordion>
      </div>
    </OnboardingShell>
  );
}

function ImageStatusBadge({ status }: { status: ImageBuildStatus }) {
  switch (status) {
    case "ready":
      return (
        <Badge className="border-emerald-500/30 bg-emerald-500/10 text-emerald-300">
          Ready
        </Badge>
      );
    case "building":
      return (
        <Badge className="border-sky-500/30 bg-sky-500/10 text-sky-300">
          <HugeiconsIcon icon={Loading02Icon} className="animate-spin" />
          Building
        </Badge>
      );
    case "failed":
      return <Badge variant="destructive">Failed</Badge>;
    default:
      return <Badge variant="outline">Not built</Badge>;
  }
}

function OnboardingShell({
  children,
  complete = false,
}: {
  children: ReactNode;
  complete?: boolean;
}) {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center bg-background px-6 py-12 text-foreground">
      <div className="flex w-full max-w-2xl flex-col items-center gap-8">
        <div className="relative">
          <div
            className="pointer-events-none absolute left-1/2 top-1/2 h-16 w-16 -translate-x-1/2 -translate-y-1/2 rounded-full bg-purple-400/40"
            style={{ filter: "blur(40px)" }}
          />
          <img
            src={logoSvg}
            alt="Djinn"
            className="relative h-16 w-auto drop-shadow-[0_0_40px_rgba(168,139,250,0.35)]"
          />
        </div>
        <OnboardingProgress current="environment" complete={complete} />
        <div className="w-full">{children}</div>
      </div>
    </main>
  );
}
