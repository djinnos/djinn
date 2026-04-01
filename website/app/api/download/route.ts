import { NextRequest, NextResponse } from "next/server";

const REPO = "djinnos/djinn";
const GITHUB_API = `https://api.github.com/repos/${REPO}/releases`;

// Map platform params to asset filename patterns
const PLATFORM_PATTERNS: Record<string, RegExp> = {
  "mac-arm64": /Djinn-.*arm64\.dmg$/,
  "windows": /Djinn\.Setup\..*\.exe$/,
  "linux-appimage": /Djinn-.*\.AppImage$/,
  "linux-deb": /djinnos-desktop.*\.deb$/,
};

// Cache the latest release for 5 minutes to avoid hitting GitHub API rate limits
let cache: { data: GitHubRelease; timestamp: number } | null = null;
const CACHE_TTL = 5 * 60 * 1000;

interface GitHubAsset {
  name: string;
  browser_download_url: string;
}

interface GitHubRelease {
  tag_name: string;
  assets: GitHubAsset[];
}

async function getLatestDesktopRelease(): Promise<GitHubRelease> {
  if (cache && Date.now() - cache.timestamp < CACHE_TTL) {
    return cache.data;
  }

  const res = await fetch(`${GITHUB_API}?per_page=20`, {
    headers: {
      Accept: "application/vnd.github.v3+json",
      "User-Agent": "djinn-website",
    },
  });

  if (!res.ok) {
    throw new Error(`GitHub API error: ${res.status}`);
  }

  const releases: GitHubRelease[] = await res.json();
  const desktopRelease = releases.find((r) =>
    r.tag_name.startsWith("desktop-v")
  );

  if (!desktopRelease) {
    throw new Error("No desktop release found");
  }

  cache = { data: desktopRelease, timestamp: Date.now() };
  return desktopRelease;
}

export async function GET(request: NextRequest) {
  const platform = request.nextUrl.searchParams.get("platform");

  if (!platform || !PLATFORM_PATTERNS[platform]) {
    return NextResponse.json(
      {
        error: "Invalid platform",
        valid: Object.keys(PLATFORM_PATTERNS),
      },
      { status: 400 }
    );
  }

  try {
    const release = await getLatestDesktopRelease();
    const pattern = PLATFORM_PATTERNS[platform];
    const asset = release.assets.find((a) => pattern.test(a.name));

    if (!asset) {
      // Fallback to releases page if asset not found
      return NextResponse.redirect(
        `https://github.com/${REPO}/releases`
      );
    }

    return NextResponse.redirect(asset.browser_download_url);
  } catch {
    // On any error, redirect to releases page
    return NextResponse.redirect(
      `https://github.com/${REPO}/releases`
    );
  }
}
