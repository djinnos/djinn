/**
 * Small round avatar for an org user: GitHub profile picture when available,
 * else the display-name initial, else a robot glyph for an unknown/automation
 * user. Shared by the board owner filter, proposals, and anywhere a person is
 * shown next to their name.
 */
import { HugeiconsIcon } from "@hugeicons/react";
import { Robot01Icon } from "@hugeicons/core-free-icons";
import { type OrgUser, userDisplayName } from "@/api/users";
import { cn } from "@/lib/utils";

export function UserAvatar({
  user,
  className,
}: {
  user?: OrgUser;
  className?: string;
}) {
  if (!user) {
    return (
      <span
        className={cn(
          "flex size-5 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground",
          className
        )}
      >
        <HugeiconsIcon icon={Robot01Icon} className="size-3" />
      </span>
    );
  }
  if (user.github_avatar_url) {
    return (
      <img
        src={user.github_avatar_url}
        alt=""
        className={cn("size-5 shrink-0 rounded-full", className)}
      />
    );
  }
  return (
    <span
      className={cn(
        "flex size-5 shrink-0 items-center justify-center rounded-full bg-muted text-[10px] font-medium text-foreground",
        className
      )}
    >
      {userDisplayName(user).charAt(0).toUpperCase()}
    </span>
  );
}
