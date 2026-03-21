import { useState } from "react";
import { selectDirectory } from "@/tauri/commands";
import { addProject } from "@/api/server";
import { useWizardStore } from "@/stores/wizardStore";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Field,
  FieldLabel,
  FieldDescription,

} from "@/components/ui/field";
import { CheckmarkCircle04Icon, AlertCircleIcon, Loading02Icon, Folder02Icon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

export function ProjectSetupStep() {
  const [selectedPath, setSelectedPath] = useState<string>("");
  const [projectName, setProjectName] = useState<string>("");
  const [isSelecting, setIsSelecting] = useState(false);
  const [isRegistering, setIsRegistering] = useState(false);
  const [isRegistered, setIsRegistered] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { nextStep } = useWizardStore();

  // Handle directory selection via native dialog
  const handleSelectDirectory = async () => {
    setIsSelecting(true);
    setError(null);
    try {
      const path = await selectDirectory("Select Project Directory");
      if (path) {
        setSelectedPath(path);
        // Auto-generate project name from directory name
        const dirName = path.split(/[/\\]/).pop() || "";
        if (!projectName) {
          setProjectName(dirName);
        }
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to open directory picker");
    } finally {
      setIsSelecting(false);
    }
  };

  // Register the project with the server
  const handleRegisterProject = async () => {
    if (!selectedPath) return;

    setIsRegistering(true);
    setError(null);
    try {
      await addProject(selectedPath);
      setIsRegistered(true);
      // Advance to next step after a brief delay to show success
      setTimeout(() => {
        nextStep();
      }, 1000);
    } catch (err) {
      setIsRegistered(false);
      setError(err instanceof Error ? err.message : "Failed to register project");
    } finally {
      setIsRegistering(false);
    }
  };

  return (
    <div className="flex flex-col gap-6">
      <div className="text-center">
        <h2 className="text-2xl font-semibold">Set Up Your Project</h2>
        <p className="text-muted-foreground">
          Select a directory to register as your first project.
        </p>
      </div>

      <div className="flex flex-col gap-4">
        {/* Directory Selection */}
        <Field>
          <FieldLabel>Project Directory</FieldLabel>
          <div className="flex gap-2">
            <Input
              placeholder="Select a directory..."
              value={selectedPath}
              readOnly
              className="flex-1"
            />
            <Button
              onClick={handleSelectDirectory}
              disabled={isSelecting}
              variant="secondary"
            >
              {isSelecting ? (
                <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
              ) : (
                <>
                  <HugeiconsIcon icon={Folder02Icon} size={16} className="mr-2" />
                  Browse
                </>
              )}
            </Button>
          </div>
          <FieldDescription>
            Choose the root directory for your project.
          </FieldDescription>
        </Field>

        {/* Project Name (optional, auto-populated) */}
        {selectedPath && (
          <Field>
            <FieldLabel>Project Name</FieldLabel>
            <Input
              placeholder="Enter project name..."
              value={projectName}
              onChange={(e) => setProjectName(e.target.value)}
            />
            <FieldDescription>
              This name will be used to identify your project.
            </FieldDescription>
          </Field>
        )}

        {/* Error Display */}
        {error && (
          <div className="flex items-center gap-2 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
            <HugeiconsIcon icon={AlertCircleIcon} size={16} className="flex-shrink-0" />
            <span>{error}</span>
          </div>
        )}

        {/* Success Display */}
        {isRegistered && (
          <div className="flex items-center gap-2 rounded-md bg-green-500/10 p-3 text-sm text-green-600">
            <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="flex-shrink-0" />
            <span>Project registered successfully!</span>
          </div>
        )}

        {/* Register Button */}
        <Button
          onClick={handleRegisterProject}
          disabled={!selectedPath || isRegistering || isRegistered}
          className="w-full"
        >
          {isRegistering ? (
            <>
              <HugeiconsIcon icon={Loading02Icon} size={16} className="mr-2 animate-spin" />
              Registering...
            </>
          ) : isRegistered ? (
            <>
              <HugeiconsIcon icon={CheckmarkCircle04Icon} size={16} className="mr-2" />
              Registered
            </>
          ) : (
            "Register Project"
          )}
        </Button>
      </div>
    </div>
  );
}
