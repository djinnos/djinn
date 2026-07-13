import { describe, expect, it } from "vitest";

import { normalizeConfig } from "@/api/environmentConfig";
import type { CatalogImage } from "@/api/images";
import type { Stack } from "@/api/devcontainer";

import {
  catalogConfigFromStack,
  findReusableImage,
  recommendedImageName,
} from "./projectImageRecommendation";

describe("project image recommendation", () => {
  it("names detected Node images with their version and package manager", () => {
    const config = catalogConfigFromStack({
      runtimes: { node: "22" },
      package_managers: ["npm"],
      workspaces: [{ root: "", language: "node", package_manager: "npm" }],
    });

    expect(recommendedImageName(config)).toBe("Node 22 + npm");
  });

  it("creates a clean shared config from stack metadata only", () => {
    const stack = {
      runtimes: { rust: "1.88", node: "22" },
      package_managers: ["npm"],
      workspaces: [
        { root: "server", language: "rust", toolchain: "1.88" },
        { root: "ui", language: "node", toolchain: "22", package_manager: "npm" },
      ],
    } satisfies Stack;

    expect(catalogConfigFromStack(stack)).toEqual({
      schema_version: 1,
      source: "auto-detected",
      languages: {
        rust: { default_toolchain: "1.88" },
        node: { default_version: "22", default_package_manager: "npm" },
      },
      workspaces: [],
      system_packages: [],
      env: {},
      lifecycle: { post_build: [], pre_anything: [], pre_task: [] },
    });
  });

  it("supports manifest-backed Java and Ruby images with builder-safe defaults", () => {
    const config = catalogConfigFromStack({
      workspaces: [
        { root: "backend", language: "java" },
        { root: "web", language: "ruby" },
      ],
    });

    expect(config.languages.java).toEqual({ default_version: "21" });
    expect(config.languages.ruby).toEqual({ default_version: "3.3.0" });
    expect(recommendedImageName(config)).toBe("Java 21 + Ruby 3.3.0");
  });

  it("prefers a workspace toolchain when no top-level runtime was detected", () => {
    const config = catalogConfigFromStack({
      workspaces: [{ root: "app", language: "python", toolchain: "3.13" }],
    });

    expect(config.languages.python).toEqual({ default_version: "3.13" });
  });

  it("reuses only a prior image with the same shared configuration", () => {
    const config = normalizeConfig({
      schema_version: 1,
      languages: {
        node: { default_version: "22", default_package_manager: "npm" },
      },
    });
    const image = {
      id: "image-1",
      name: "Node 22 + npm",
      status: "ready",
      config,
      servicePresets: [],
    } satisfies CatalogImage;

    expect(findReusableImage([image], config)).toBe(image);
    expect(
      findReusableImage(
        [image],
        normalizeConfig({
          schema_version: 1,
          languages: { go: { default_version: "1.24" } },
        }),
      ),
    ).toBeUndefined();
  });

  it("does not auto-reuse an image that injects service presets", () => {
    const config = catalogConfigFromStack({
      runtimes: { node: "22" },
      package_managers: ["npm"],
      workspaces: [{ root: "", language: "node", package_manager: "npm" }],
    });
    const image = {
      id: "image-with-postgres",
      name: "Node 22 + npm + Postgres",
      status: "ready",
      config,
      servicePresets: ["postgres"],
    } satisfies CatalogImage;

    expect(findReusableImage([image], config)).toBeUndefined();
  });

  it("does not auto-reuse an image whose build failed", () => {
    const config = catalogConfigFromStack({
      runtimes: { node: "22" },
      package_managers: ["npm"],
      workspaces: [{ root: "", language: "node", package_manager: "npm" }],
    });
    const image = {
      id: "failed-image",
      name: "Node 22 + npm",
      status: "failed",
      config,
      servicePresets: [],
    } satisfies CatalogImage;

    expect(findReusableImage([image], config)).toBeUndefined();
  });
});
