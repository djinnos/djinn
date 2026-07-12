import { type ReactNode, useMemo, useRef, useState } from "react";
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
} from "@/api/images";
import { fetchDevcontainerStatus, fetchProjectStack } from "@/api/devcontainer";
import {
  fetchEnvironmentConfig,
  resetEnvironmentConfig,
} from "@/api/environmentConfig";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { InlineError } from "@/components/InlineError";

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
  const detectedLanguages = useMemo(() => {
    const fromStack = (stackQuery.data?.stack?.languages ?? [])
      .map((language) => language.name)
      .filter(Boolean);
    if (fromStack.length > 0) return fromStack.join(", ");
    return Object.entries(environmentQuery.data?.config.languages ?? {})
      .filter(([, value]) => value != null)
      .map(([language]) => language)
      .join(", ");
  }, [environmentQuery.data?.config.languages, stackQuery.data?.stack?.languages]);

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
      // Stack detection is not required to reuse an image. When it has already
      // completed, though, persist this project's detected workspaces now so a
      // fresh project does not have to wait for a server restart/boot reseed.
      if (stackQuery.data?.stack) {
        await ensureDetectedProjectConfig();
      }
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
      const config = catalogConfigFromStack(stack);
      const preferredName = recommendedImageName(config);
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
      <OnboardingShell>
        <div className="flex flex-col items-center gap-5 text-center">
          <div className="flex h-12 w-12 items-center justify-center rounded-full bg-primary/15">
            <HugeiconsIcon
              icon={CheckmarkCircle04Icon}
              size={26}
              className="text-primary"
            />
          </div>
          <div>
            <h2 className="text-xl font-semibold">Environment build started</h2>
            <p className="mt-1 text-sm text-muted-foreground">
              {assignedName} is assigned to {project.name}. You can enter Djinn
              now; agents become available when the image build finishes.
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
          <h2 className="text-xl font-semibold">Prepare the runtime environment</h2>
          <p className="mt-1 text-sm text-muted-foreground">
            {project.name} needs a shared catalog image before agents can run.
            Reuse an existing image or let Djinn create one from the detected stack.
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

        {images.length > 0 && (
          <section className="rounded-xl border bg-card/30 p-5">
            <div className="flex items-start gap-3">
              <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-primary/15">
                <HugeiconsIcon icon={CubeIcon} size={20} className="text-primary" />
              </div>
              <div className="min-w-0 flex-1">
                <h3 className="text-sm font-semibold">Use an existing image</h3>
                <p className="mt-0.5 text-xs text-muted-foreground">
                  Best when another repository already uses the same toolchain.
                </p>
                <div className="mt-3 flex items-center gap-2">
                  <Select
                    value={selectedImageId || null}
                    onValueChange={(value) => {
                      if (typeof value === "string") setSelectedImageId(value);
                    }}
                    disabled={phase === "saving"}
                  >
                    <SelectTrigger className="min-w-64 flex-1">
                      <SelectValue placeholder="Select a catalog image" />
                    </SelectTrigger>
                    <SelectContent>
                      {images.map((image) => (
                        <SelectItem key={image.id} value={image.id}>
                          {image.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <Button
                    variant="outline"
                    disabled={!selectedImage || phase === "saving"}
                    onClick={() => void assignExisting()}
                  >
                    Use image
                  </Button>
                </div>
              </div>
            </div>
          </section>
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
              <h3 className="text-sm font-semibold">Create from detected stack</h3>
              <p className="mt-0.5 text-xs text-muted-foreground">
                {canPrepareDetectedImage
                  ? `Detected ${detectedLanguages || "a base environment"}. Djinn will preserve repository workspaces separately and create one reusable image.`
                  : detectionTimedOut
                    ? "Detection is taking longer than expected. Retry after the repository mirror finishes."
                    : "Cloning the repository and detecting languages, versions, and package managers…"}
              </p>
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
                {phase === "saving" ? "Preparing environment…" : "Create detected image"}
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
      </div>
    </OnboardingShell>
  );
}

function OnboardingShell({ children }: { children: ReactNode }) {
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
        <div className="w-full">{children}</div>
      </div>
    </main>
  );
}
