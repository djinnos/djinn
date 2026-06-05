/**
 * Small presentational helpers for catalog images.
 */
import {
  type EnvironmentConfig,
  type LanguageKey,
  LANGUAGE_KEYS,
} from "@/api/environmentConfig";

const LANGUAGE_LABELS: Record<LanguageKey, string> = {
  rust: "Rust",
  node: "Node",
  python: "Python",
  go: "Go",
  java: "Java",
  ruby: "Ruby",
  dotnet: ".NET",
  clang: "Clang",
};

/** Comma-joined display labels of the languages an image enables. */
export function listEnabledLanguages(config: EnvironmentConfig): string {
  return LANGUAGE_KEYS.filter((key) => config.languages[key] !== undefined)
    .map((key) => LANGUAGE_LABELS[key])
    .join(", ");
}
