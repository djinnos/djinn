import type { CatalogImage } from "@/api/images";
import type { Stack, StackWorkspace } from "@/api/devcontainer";
import {
  normalizeConfig,
  type EnvironmentConfig,
  type Languages,
} from "@/api/environmentConfig";

const DEFAULT_RUST_TOOLCHAIN = "stable";
const DEFAULT_NODE_VERSION = "22";
const DEFAULT_PYTHON_VERSION = "3.12";
const DEFAULT_GO_VERSION = "1.22";
// These match the versions documented by the image-builder install scripts.
const DEFAULT_JAVA_VERSION = "21";
const DEFAULT_RUBY_VERSION = "3.3.0";

function workspaceFor(stack: Stack, language: string): StackWorkspace | undefined {
  return (stack.workspaces ?? []).find((workspace) => workspace.language === language);
}

function detectedVersion(
  runtime: string | null | undefined,
  workspace: StackWorkspace | undefined,
  fallback: string,
): string {
  return runtime?.trim() || workspace?.toolchain?.trim() || fallback;
}

/**
 * Build the reusable catalog-image config from stack metadata alone.
 *
 * Catalog images are shared across projects. Project configuration can contain
 * secrets, hooks, system packages, MCP/skill preferences, Cargo policy, and
 * repository-specific workspace paths, so it must never be used as the source
 * for an automatically-created shared image. Stack metadata is the narrow,
 * read-only input that contains only detected toolchains and package managers.
 */
export function catalogConfigFromStack(stack: Stack): EnvironmentConfig {
  const languages: Languages = {};
  const rust = workspaceFor(stack, "rust");
  const node = workspaceFor(stack, "node");
  const python = workspaceFor(stack, "python");
  const go = workspaceFor(stack, "go");
  const java = workspaceFor(stack, "java");
  const ruby = workspaceFor(stack, "ruby");

  if (stack.runtimes?.rust || rust) {
    languages.rust = {
      default_toolchain: detectedVersion(
        stack.runtimes?.rust,
        rust,
        DEFAULT_RUST_TOOLCHAIN,
      ),
    };
  }
  if (stack.runtimes?.node || node) {
    const packageManager =
      node?.package_manager?.trim() ||
      (stack.package_managers ?? []).find((manager) =>
        ["pnpm", "yarn", "bun", "npm"].includes(manager),
      ) ||
      "pnpm";
    languages.node = {
      default_version: detectedVersion(
        stack.runtimes?.node,
        node,
        DEFAULT_NODE_VERSION,
      ),
      default_package_manager: packageManager,
    };
  }
  if (stack.runtimes?.python || python) {
    languages.python = {
      default_version: detectedVersion(
        stack.runtimes?.python,
        python,
        DEFAULT_PYTHON_VERSION,
      ),
    };
  }
  if (stack.runtimes?.go || go) {
    languages.go = {
      default_version: detectedVersion(stack.runtimes?.go, go, DEFAULT_GO_VERSION),
    };
  }
  if (java) {
    languages.java = {
      default_version: detectedVersion(undefined, java, DEFAULT_JAVA_VERSION),
    };
  }
  if (ruby) {
    languages.ruby = {
      default_version: detectedVersion(undefined, ruby, DEFAULT_RUBY_VERSION),
    };
  }

  return normalizeConfig({
    schema_version: 1,
    source: "auto-detected",
    languages,
  });
}

/** Build a stable, human-readable name for an auto-detected catalog image. */
export function recommendedImageName(config: EnvironmentConfig): string {
  const labels: string[] = [];
  const { languages } = config;

  if (languages.rust) {
    labels.push(`Rust ${languages.rust.default_toolchain}`);
  }
  if (languages.node) {
    labels.push(`Node ${languages.node.default_version}`);
    if (languages.node.default_package_manager) {
      labels.push(languages.node.default_package_manager);
    }
  }
  if (languages.python) {
    labels.push(`Python ${languages.python.default_version}`);
  }
  if (languages.go) {
    labels.push(`Go ${languages.go.default_version}`);
  }
  if (languages.java) {
    labels.push(`Java ${languages.java.default_version}`);
  }
  if (languages.ruby) {
    labels.push(`Ruby ${languages.ruby.default_version}`);
  }
  if (languages.dotnet) {
    labels.push(`.NET ${languages.dotnet.default_version}`);
  }
  if (languages.clang) {
    labels.push(`Clang ${languages.clang.default_version}`);
  }

  return labels.length > 0 ? labels.join(" + ") : "Base environment";
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(",")}]`;
  }
  if (value && typeof value === "object") {
    const entries = Object.entries(value)
      .filter(([, child]) => child !== undefined)
      .sort(([left], [right]) => left.localeCompare(right));
    return `{${entries
      .map(([key, child]) => `${JSON.stringify(key)}:${canonicalJson(child)}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Reuse only a semantically identical image after a retry or page reload. */
export function findReusableImage(
  images: CatalogImage[],
  config: EnvironmentConfig,
): CatalogImage | undefined {
  const wanted = canonicalJson(config);
  return images.find(
    (image) =>
      image.status !== "failed" &&
      image.servicePresets.length === 0 &&
      canonicalJson(image.config) === wanted,
  );
}

/**
 * Keep the concise detected name when available. If another configuration
 * already owns it, qualify the name by repository and then a numeric suffix.
 */
export function availableImageName(
  images: CatalogImage[],
  preferred: string,
  projectSlug: string,
): string {
  const names = new Set(images.map((image) => image.name));
  if (!names.has(preferred)) return preferred;

  const qualified = `${preferred} (${projectSlug})`;
  if (!names.has(qualified)) return qualified;

  let suffix = 2;
  while (names.has(`${qualified} ${suffix}`)) suffix += 1;
  return `${qualified} ${suffix}`;
}
