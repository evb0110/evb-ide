import { parseRelease } from "../../../shared/releases";

export default defineCachedEventHandler(
  async () => {
    const { githubToken } = useRuntimeConfig();
    try {
      const response = await fetch("https://api.github.com/repos/evb0110/evb-ide/releases/latest", {
        headers: {
          Accept: "application/vnd.github+json",
          "User-Agent": "EVB-IDE-landing",
          ...(githubToken ? { Authorization: `Bearer ${githubToken}` } : {}),
        },
        signal: AbortSignal.timeout(8000),
      });
      if (response.status === 404) {
        return { release: null };
      }
      if (!response.ok) {
        throw new Error(`GitHub release lookup returned HTTP ${response.status}`);
      }
      const release = parseRelease(await response.json());
      if (!release) {
        throw new Error("The latest release has no complete source and installer asset set");
      }
      return { release };
    } catch (error) {
      console.error("Unable to load EVB IDE releases:", error instanceof Error ? error.message : "unknown error");
      throw createError({
        statusCode: 503,
        statusMessage: "Release information is temporarily unavailable. View downloads on GitHub.",
      });
    }
  },
  { maxAge: 300, swr: false },
);
