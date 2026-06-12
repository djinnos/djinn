/** Known provider display names; falls back to title-casing the id. */
export function formatProvider(id: string): string {
  const known: Record<string, string> = {
    openai: "OpenAI",
    anthropic: "Anthropic",
    google: "Google",
    azure: "Azure",
    aws: "AWS",
    "fireworks-ai": "Fireworks",
    mistral: "Mistral",
    cohere: "Cohere",
    groq: "Groq",
    deepseek: "DeepSeek",
    perplexity: "Perplexity",
    chatgpt_codex: "ChatGPT / Codex",
  };
  return (
    known[id.toLowerCase()] ??
    id.replace(/[-_]/g, " ").replace(/\b\w/g, (c) => c.toUpperCase())
  );
}
