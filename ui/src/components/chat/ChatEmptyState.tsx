import logoSvg from '@/assets/logo.svg';

interface ChatEmptyStateProps {
  onPromptClick: (prompt: string) => void;
}

const SUGGESTED_PROMPTS = [
  'Show me my epics',
  'Create a task for adding chat tests',
  "What's the status of my project?",
  'Plan the next milestone',
];

export function ChatEmptyState({ onPromptClick }: ChatEmptyStateProps) {
  return (
    <div className="flex h-full flex-1 items-center justify-center p-8">
      <div className="max-w-xl text-center">
        <img src={logoSvg} alt="Djinn" className="mx-auto mb-4 h-10 w-10" />
        <h2 className="text-2xl font-semibold">Chat</h2>
        <p className="mt-2 text-muted-foreground">Chat about your project or plan your next milestone</p>
        <div className="mt-6 grid grid-cols-1 gap-2 sm:grid-cols-2">
          {SUGGESTED_PROMPTS.map((prompt) => (
            <button
              key={prompt}
              type="button"
              className="rounded-md border border-border bg-card px-3 py-2 text-left text-sm hover:bg-muted"
              onClick={() => onPromptClick(prompt)}
            >
              {prompt}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
