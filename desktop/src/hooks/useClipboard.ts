import { useCallback, useState } from "react";

interface UseClipboardOptions {
  feedbackDuration?: number;
}

export function useClipboard({ feedbackDuration = 1500 }: UseClipboardOptions = {}) {
  const [copied, setCopied] = useState(false);

  const copy = useCallback(
    async (text: string): Promise<void> => {
      try {
        await navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), feedbackDuration);
      } catch (error) {
        console.error("Failed to copy to clipboard:", error);
      }
    },
    [feedbackDuration],
  );

  return { copy, copied };
}
