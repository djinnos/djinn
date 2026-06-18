import { SparklesIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import { userDisplayName, type OrgUser } from "@/api/users";
import { ModelSection } from "@/components/userConfig/ModelSection";
import { ProviderSection } from "@/components/userConfig/ProviderSection";

interface UserConfigDialogProps {
  /** The target user — its `id` is threaded as `target_user_id`. */
  user: OrgUser;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function UserConfigDialog({ user, open, onOpenChange }: UserConfigDialogProps) {
  const targetId = user.id;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] w-full overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <HugeiconsIcon icon={SparklesIcon} size={18} className="text-primary" />
            Configure user
          </DialogTitle>
          <DialogDescription>
            Manage credentials and model selection for{" "}
            <span className="font-medium text-foreground">{userDisplayName(user)}</span>
            . Configure this user&apos;s providers, models, and credentials on
            their behalf.
          </DialogDescription>
        </DialogHeader>

        {open && (
          <div className="flex flex-col gap-6">
            <ProviderSection targetId={targetId} />
            <Separator />
            <ModelSection targetId={targetId} />
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
