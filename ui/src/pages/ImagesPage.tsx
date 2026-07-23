/**
 * ImagesPage — `/images`.
 *
 * Lists the shared image catalog (the `images` table): reusable
 * `EnvironmentConfig` presets a project can adopt instead of authoring its
 * own. Create/edit navigate to the full-page ImageEditorPage; delete with a confirm (the
 * server rejects deleting an image a project still uses).
 */
import { useCallback, useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  CubeIcon,
  Delete02Icon,
  Edit02Icon,
  Loading02Icon,
  PlusSignIcon,
} from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { EmptyState } from "@/components/EmptyState";
import { RepositoriesSectionTabs } from "@/components/RepositoriesSectionTabs";
import {
  deleteImage,
  listImages,
  listServicePresets,
  type CatalogImage,
  type ImageBuildStatus,
  type ServicePreset,
} from "@/api/images";
import { listEnabledLanguages } from "@/components/images/imageSummary";
import { showToast } from "@/lib/toast";

function StatusBadge({ status }: { status: ImageBuildStatus }) {
  switch (status) {
    case "ready":
      return (
        <Badge className="border-emerald-500/30 bg-emerald-500/10 text-emerald-300">Ready</Badge>
      );
    case "building":
      return (
        <Badge className="border-sky-500/30 bg-sky-500/10 text-sky-300">
          <HugeiconsIcon icon={Loading02Icon} className="animate-spin" />
          Building
        </Badge>
      );
    case "failed":
      return <Badge variant="destructive">Build failed</Badge>;
    default:
      return <Badge variant="outline">Not built</Badge>;
  }
}

function DeleteImageButton({
  image,
  onDeleted,
}: {
  image: CatalogImage;
  onDeleted: () => Promise<void> | void;
}) {
  const [open, setOpen] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const handleConfirm = async () => {
    setDeleting(true);
    try {
      const result = await deleteImage(image.id);
      if (!result.ok) {
        showToast.error("Could not delete image", { description: result.error });
        return;
      }
      showToast.success(`Deleted ${image.name}`);
      await onDeleted();
      setOpen(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to delete image";
      showToast.error("Could not delete image", { description: message });
    } finally {
      setDeleting(false);
    }
  };

  return (
    <AlertDialog open={open} onOpenChange={(v) => !deleting && setOpen(v)}>
      <AlertDialogTrigger
        render={
          <button
            type="button"
            className="flex h-7 w-7 items-center justify-center rounded-md border bg-transparent text-muted-foreground transition-colors hover:border-red-400/40 hover:bg-red-500/10 hover:text-red-400"
            title={`Delete ${image.name}`}
          />
        }
      >
        <HugeiconsIcon icon={Delete02Icon} size={14} />
      </AlertDialogTrigger>
      <AlertDialogContent size="sm">
        <AlertDialogHeader>
          <AlertDialogTitle>Delete {image.name}?</AlertDialogTitle>
          <AlertDialogDescription>
            This removes the image from the catalog. It fails if any project is still using it —
            point those projects at a different image first.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={deleting}>Cancel</AlertDialogCancel>
          <Button variant="destructive" disabled={deleting} onClick={() => void handleConfirm()}>
            {deleting ? (
              <>
                <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                Deleting…
              </>
            ) : (
              "Delete"
            )}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

export function ImagesPage() {
  const navigate = useNavigate();
  const [images, setImages] = useState<CatalogImage[]>([]);
  const [presetsById, setPresetsById] = useState<Record<string, ServicePreset>>({});
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [loadedImages, presets] = await Promise.all([
        listImages(),
        listServicePresets(),
      ]);
      setImages(loadedImages);
      setPresetsById(Object.fromEntries(presets.map((p) => [p.id, p])));
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to load images";
      showToast.error("Could not load images", { description: message });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const openCreate = () => navigate("/images/new");
  const openEdit = (image: CatalogImage) => navigate(`/images/${image.id}/edit`);

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between border-b px-6 py-4">
        <div>
          <h1 className="text-lg font-semibold">Images</h1>
          <p className="text-sm text-muted-foreground">
            Reusable environment presets a project can adopt instead of its own config.
          </p>
        </div>
        <Button onClick={openCreate}>
          <HugeiconsIcon icon={PlusSignIcon} className="h-4 w-4" />
          New image
        </Button>
      </header>

      <div className="border-b px-6 py-2">
        <RepositoriesSectionTabs active="images" />
      </div>

      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto max-w-5xl">
          {loading ? (
            <div className="flex h-40 items-center justify-center">
              <HugeiconsIcon
                icon={Loading02Icon}
                className="size-5 animate-spin text-muted-foreground"
              />
            </div>
          ) : images.length === 0 ? (
            <EmptyState
              title="No images yet"
              message="Create a reusable environment preset to share across projects."
              actionLabel="New image"
              onAction={openCreate}
              illustration={<HugeiconsIcon icon={CubeIcon} className="h-10 w-10" />}
            />
          ) : (
            <div className="overflow-hidden rounded-md border">
              <table className="w-full text-left">
                <thead className="bg-white/[0.02]">
                  <tr className="border-b text-xs uppercase tracking-wide text-muted-foreground">
                    <th className="px-4 py-2.5 font-medium">Name</th>
                    <th className="px-4 py-2.5 font-medium">Languages</th>
                    <th className="px-4 py-2.5 font-medium">Services</th>
                    <th className="px-4 py-2.5 font-medium">Status</th>
                    <th className="px-4 py-2.5 text-right font-medium">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {images.map((image) => (
                    <tr
                      key={image.id}
                      className="border-b border-border/50 transition-colors hover:bg-white/[0.03]"
                    >
                      <td className="px-4 py-3">
                        <div className="flex items-center gap-3">
                          <HugeiconsIcon
                            icon={CubeIcon}
                            className="h-4 w-4 shrink-0 text-muted-foreground"
                          />
                          <div className="min-w-0">
                            <div className="truncate text-sm font-medium text-foreground">
                              {image.name}
                            </div>
                            {image.description && (
                              <div className="truncate text-xs text-muted-foreground">
                                {image.description}
                              </div>
                            )}
                          </div>
                        </div>
                      </td>
                      <td className="px-4 py-3">
                        <span className="text-xs text-muted-foreground">
                          {listEnabledLanguages(image.config) || "—"}
                        </span>
                      </td>
                      <td className="px-4 py-3">
                        {image.servicePresets.length === 0 ? (
                          <span className="text-xs text-muted-foreground">—</span>
                        ) : (
                          <div className="flex flex-wrap gap-1">
                            {image.servicePresets.map((presetId) => (
                              <Badge
                                key={presetId}
                                variant="outline"
                                className="text-xs font-normal"
                              >
                                {presetsById[presetId]?.name ?? presetId}
                              </Badge>
                            ))}
                          </div>
                        )}
                      </td>
                      <td className="px-4 py-3">
                        <StatusBadge status={image.status} />
                      </td>
                      <td className="px-4 py-3">
                        <div className="flex items-center justify-end gap-1.5">
                          <button
                            type="button"
                            onClick={() => openEdit(image)}
                            className="flex h-7 w-7 items-center justify-center rounded-md border bg-transparent text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground"
                            title="Edit image"
                          >
                            <HugeiconsIcon icon={Edit02Icon} size={14} />
                          </button>
                          <DeleteImageButton image={image} onDeleted={load} />
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
