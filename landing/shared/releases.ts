export const repositoryUrl = "https://github.com/evb0110/evb-ide";

export interface IInstaller {
  name: string;
  url: string;
  size: number;
  architecture: "arm64" | "x64";
}

export interface IRelease {
  tag: string;
  url: string;
  publishedAt: string;
  sourceUrl: string;
  checksumsUrl: string;
  installers: IInstaller[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function parseRelease(value: unknown): IRelease | null {
  if (
    !isRecord(value) ||
    value.draft !== false ||
    value.prerelease !== false ||
    typeof value.tag_name !== "string" ||
    !/^v\d+\.\d+\.\d+$/.test(value.tag_name) ||
    typeof value.published_at !== "string" ||
    !Number.isFinite(Date.parse(value.published_at)) ||
    !Array.isArray(value.assets)
  ) {
    return null;
  }

  const tag = value.tag_name;
  const downloadBase = `${repositoryUrl}/releases/download/${tag}/`;
  const assets = value.assets.filter(
    (asset): asset is Record<string, unknown> =>
      isRecord(asset) &&
      typeof asset.name === "string" &&
      typeof asset.browser_download_url === "string" &&
      asset.browser_download_url === `${downloadBase}${asset.name}` &&
      typeof asset.size === "number" &&
      Number.isSafeInteger(asset.size) &&
      asset.size > 0,
  );
  const checksums = assets.find((asset) => asset.name === "SHA256SUMS");
  const source = assets.find((asset) => asset.name === `EVB-IDE-${tag.slice(1)}-source.tar.gz`);
  if (!checksums || !source) {
    return null;
  }

  const installers: IInstaller[] = [];
  for (const asset of assets) {
    const architecture =
      asset.name === `EVB-IDE-${tag.slice(1)}-macos-arm64.dmg`
        ? "arm64"
        : asset.name === `EVB-IDE-${tag.slice(1)}-macos-x64.dmg`
          ? "x64"
          : null;
    if (architecture) {
      installers.push({
        name: String(asset.name),
        url: String(asset.browser_download_url),
        size: Number(asset.size),
        architecture,
      });
    }
  }
  if (installers.length === 0) {
    return null;
  }
  return {
    tag,
    url: `${repositoryUrl}/releases/tag/${tag}`,
    publishedAt: value.published_at,
    sourceUrl: String(source.browser_download_url),
    checksumsUrl: String(checksums.browser_download_url),
    installers,
  };
}
