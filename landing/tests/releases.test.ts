import { test } from "node:test";
import assert from "node:assert/strict";
import { parseRelease, repositoryUrl } from "../shared/releases.ts";

function fixture() {
  const tag = "v0.1.0";
  return {
    tag_name: tag,
    published_at: "2026-10-31T08:00:00Z",
    draft: false,
    prerelease: false,
    assets: ["EVB-IDE-0.1.0-macos-arm64.dmg", "EVB-IDE-0.1.0-source.tar.gz", "SHA256SUMS"].map((name) => ({
      name,
      size: 100,
      browser_download_url: `${repositoryUrl}/releases/download/${tag}/${name}`,
    })),
  };
}

test("offers a stable Mac download with matching source and checksums", () => {
  const release = parseRelease(fixture());
  assert.equal(release?.installers.length, 1);
  assert.equal(release?.installers[0]?.architecture, "arm64");
  assert.equal(release?.sourceUrl, `${repositoryUrl}/releases/download/v0.1.0/EVB-IDE-0.1.0-source.tar.gz`);
});

test("does not offer downloads without corresponding source or checksums", () => {
  for (const missing of ["EVB-IDE-0.1.0-source.tar.gz", "SHA256SUMS"]) {
    const value = fixture();
    value.assets = value.assets.filter((asset) => asset.name !== missing);
    assert.equal(parseRelease(value), null);
  }
});

test("rejects external download URLs and assets from another version", () => {
  for (const url of [
    "https://example.com/app.dmg",
    `${repositoryUrl}/releases/download/v9.9.9/EVB-IDE-0.1.0-macos-arm64.dmg`,
  ]) {
    const value = fixture();
    const asset = value.assets.find((asset) => asset.name.endsWith(".dmg"));
    assert.ok(asset);
    asset.browser_download_url = url;
    assert.equal(parseRelease(value), null);
  }
});

test("ignores drafts, prereleases, invalid responses, and empty installers", () => {
  assert.equal(parseRelease({ ...fixture(), draft: true }), null);
  assert.equal(parseRelease({ ...fixture(), prerelease: true }), null);
  assert.equal(parseRelease({ ...fixture(), assets: [] }), null);
  assert.equal(parseRelease({ ...fixture(), tag_name: "v0.1.0/../download" }), null);
  assert.equal(parseRelease(null), null);
});
