<template>
  <div class="home">
    <section class="hero" aria-labelledby="home-title">
      <div class="hero-intro">
        <h1 id="home-title" class="hero-title">A JetBrains‑style IDE built on Zed</h1>
        <p class="hero-lede">
          EVB IDE is the Zed editor with the JetBrains keymap, IntelliJ themes, run configurations for your project's
          scripts, a Git Log tool window and GitHub pull request review. It is free and open source under the GPL.
        </p>

        <div id="download" class="installer-card" aria-live="polite">
          <template v-if="release">
            <p class="installer-version">
              <span class="installer-version-name">EVB IDE {{ release.tag.slice(1) }}</span>
              <time :datetime="release.publishedAt">{{ formatDate(release.publishedAt) }}</time>
            </p>
            <div
              v-if="platforms.length > 1"
              class="installer-tabs"
              role="tablist"
              aria-label="Operating system"
              @keydown="onTabKey"
            >
              <button
                v-for="platform in platforms"
                :key="platform"
                type="button"
                role="tab"
                class="installer-tab"
                :aria-selected="platform === selectedPlatform"
                :tabindex="platform === selectedPlatform ? 0 : -1"
                @click="chosenPlatform = platform"
              >
                {{ platformLabels[platform] }}
              </button>
            </div>
            <p v-else class="installer-platform">{{ platformLabels[selectedPlatform] }}</p>
            <a
              v-for="installer in platformInstallers"
              :key="installer.name"
              :href="installer.url"
              class="installer-row"
              :class="{ 'installer-row-recommended': installer === platformInstallers[0] }"
            >
              <span class="installer-text">
                <span class="installer-name">
                  {{ installerTitle(installer) }}
                  <span
                    v-if="installer === platformInstallers[0] && platformInstallers.length > 1"
                    class="installer-badge"
                    >Recommended</span
                  >
                </span>
                <span class="installer-meta">{{ installerMeta(installer) }} · {{ formatSize(installer.size) }}</span>
              </span>
              <span class="installer-icon"><UIcon name="i-lucide-download" /></span>
            </a>
            <p v-if="unsupportedArchitecture" class="installer-note">
              There is no build for {{ unsupportedArchitecture }} yet; the downloads below are for x64 and will not
              run on it.
            </p>
            <p class="installer-note">
              <span class="installer-requirement">{{ platformNote.requirement }}</span>
              <span v-if="platformNote.action"
                >{{ platformNote.action }} <code v-if="platformNote.file">{{ platformNote.file }}</code></span
              >
            </p>
            <p class="installer-links">
              <a target="_blank" :href="release.url">Release notes</a>
              <a :href="release.sourceUrl">Source</a>
              <a :href="release.checksumsUrl">SHA-256 checksums</a>
            </p>
          </template>
          <template v-else-if="error">
            <p class="installer-note">The latest release could not be loaded.</p>
            <UButton
              :to="`${repositoryUrl}/releases`"
              target="_blank"
              icon="i-lucide-github"
              color="neutral"
              variant="outline"
            >
              Releases on GitHub
            </UButton>
          </template>
          <p v-else-if="status === 'pending'" class="installer-note">Loading the latest release…</p>
          <template v-else>
            <p class="installer-note">No release is published yet.</p>
            <UButton :to="`${repositoryUrl}/releases`" target="_blank" color="neutral" variant="outline">
              Release status
            </UButton>
          </template>
        </div>
      </div>

      <div class="hero-film">
        <FilmPlayer
          :theme="filmTheme"
          title="EVB IDE"
          label="EVB IDE running a project script, showing the Git Log, a pull request review thread and blame"
        />
      </div>
    </section>

    <section id="features" class="section" aria-labelledby="features-title">
      <h2 id="features-title" class="section-label">What it does</h2>
      <div class="feature-grid">
        <article v-for="feature in features" :key="feature.title" class="feature-card">
          <UIcon :name="feature.icon" class="feature-icon" />
          <h3 class="feature-title">{{ feature.title }}</h3>
          <p class="feature-text">{{ feature.text }}</p>
        </article>
      </div>
    </section>

    <section class="section" aria-labelledby="questions-title">
      <h2 id="questions-title" class="section-label">Questions</h2>
      <dl class="questions">
        <div v-for="question in questions" :key="question.title" class="question">
          <dt class="question-title">{{ question.title }}</dt>
          <dd class="question-text">{{ question.text }}</dd>
        </div>
      </dl>
    </section>
  </div>
</template>

<script setup lang="ts">
import { type IInstaller, type TPlatform, repositoryUrl } from "~~/shared/releases";

interface IFeature {
  icon: string;
  title: string;
  text: string;
}

interface IPlatformNote {
  requirement: string;
  action?: string;
  file?: string;
}

interface IQuestion {
  title: string;
  text: string;
}

const { data, error, status } = await useFetch("/api/releases/latest");
const release = computed(() => data.value?.release);
const platformLabels: Record<TPlatform, string> = { macos: "macOS", linux: "Linux", windows: "Windows" };
const platformNotes: Record<TPlatform, IPlatformNote> = {
  macos: { requirement: "macOS 12 or later" },
  linux: {
    requirement: "glibc 2.35 or later, such as Ubuntu 22.04",
    action: "Extract the archive and run",
    file: "install.sh",
  },
  windows: { requirement: "Windows 10 or later", action: "Extract the archive and run", file: "evb-ide.exe" },
};
const platformNote = computed(() => platformNotes[selectedPlatform.value]);
const platforms = computed(() =>
  (["macos", "linux", "windows"] as TPlatform[]).filter((platform) =>
    release.value?.installers.some((installer) => installer.platform === platform),
  ),
);
/** The visitor's system from the User-Agent, read on the server so the page renders with it. */
const userAgent = import.meta.server ? useRequestHeaders(["user-agent"])["user-agent"] : navigator.userAgent;
const detectedPlatform = useState<TPlatform | null>("detected-platform", () => platformOf(userAgent ?? ""));
const chosenPlatform = ref<TPlatform | null>(null);
const selectedPlatform = computed<TPlatform>(() => {
  for (const platform of [chosenPlatform.value, detectedPlatform.value]) {
    if (platform && platforms.value.includes(platform)) {
      return platform;
    }
  }
  return platforms.value[0] ?? "macos";
});

// Apple Silicon on macOS and x64 elsewhere come first: the builds most visitors need.
function installerRank(installer: IInstaller) {
  const preferred = installer.platform === "macos" ? "arm64" : "x64";
  return installer.architecture === preferred ? 0 : 1;
}

const platformInstallers = computed(() =>
  (release.value?.installers ?? [])
    .filter((installer) => installer.platform === selectedPlatform.value)
    .sort((left, right) => installerRank(left) - installerRank(right)),
);

function installerTitle(installer: IInstaller) {
  if (installer.platform === "macos") {
    return installer.architecture === "arm64" ? "Apple Silicon" : "Intel";
  }
  return installer.architecture === "x64" ? "x64" : "ARM64";
}

function installerMeta(installer: IInstaller) {
  if (installer.platform === "macos") {
    return `${installer.architecture === "arm64" ? "M-series Macs" : "Intel Macs"} · DMG`;
  }
  return `${installer.architecture === "x64" ? "Intel and AMD" : "ARM"} · ${installer.format}`;
}

/** The visitor's processor when no build is published for it, such as Linux on ARM. */
const unsupportedArchitecture = computed(() => {
  const agent = (userAgent ?? "").toLowerCase();
  const arm = /aarch64|arm64|armv\d/.test(agent);
  if (!arm || selectedPlatform.value !== "linux") {
    return null;
  }
  return platformInstallers.value.some((installer) => installer.architecture !== "x64") ? null : "Linux on ARM";
});

// Arrow keys, Home and End move between the system tabs, as the tabs pattern expects.
function onTabKey(event: KeyboardEvent) {
  const index = platforms.value.indexOf(selectedPlatform.value);
  const last = platforms.value.length - 1;
  const next = { ArrowLeft: index - 1, ArrowRight: index + 1, Home: 0, End: last }[event.key];
  if (next === undefined) {
    return;
  }
  event.preventDefault();
  const platform = platforms.value[(next + platforms.value.length) % platforms.value.length];
  if (!platform) {
    return;
  }
  chosenPlatform.value = platform;
  nextTick(() => {
    const tabs = (event.currentTarget as HTMLElement | null)?.querySelectorAll<HTMLElement>("[role=tab]");
    tabs?.[platforms.value.indexOf(platform)]?.focus();
  });
}

function platformOf(userAgent: string): TPlatform | null {
  const agent = userAgent.toLowerCase();
  if (agent.includes("windows")) {
    return "windows";
  }
  if (agent.includes("linux") && !agent.includes("android")) {
    return "linux";
  }
  return agent.includes("mac") ? "macos" : null;
}
const colorMode = useColorMode();
const filmTheme = computed(() => (colorMode.value === "dark" ? "dark" : "light"));

const features: IFeature[] = [
  {
    icon: "i-lucide-keyboard",
    title: "JetBrains keymap and themes",
    text: "The JetBrains keymap, IntelliJ Light and Dark, JetBrains file icons, JetBrains Mono and Inter, set up from the first launch.",
  },
  {
    icon: "i-lucide-play",
    title: "Run configurations",
    text: "package.json and Composer scripts and Zed tasks in a title-bar picker. Run, rerun and stop them, or debug a Node.js script.",
  },
  {
    icon: "i-lucide-git-branch",
    title: "Git Log tool window",
    text: "The log below the editor, with a branches tree, filters by branch, user, date and path, and cherry-pick, revert, reset, fixup, drop and reword.",
  },
  {
    icon: "i-lucide-git-pull-request",
    title: "GitHub pull requests",
    text: "The branch's pull request and its checks in the status bar. Create one after a push, and reply to and resolve review threads in the editor.",
  },
  {
    icon: "i-lucide-history",
    title: "Blame and history",
    text: "A compact blame column with the date and author, file history, and Commit and Push from the commit button.",
  },
  {
    icon: "i-lucide-code-xml",
    title: "Set up for web projects",
    text: "Defaults for Vue, TypeScript, PHP and Blade, with completion and rename working across several language servers.",
  },
];

const questions: IQuestion[] = [
  {
    title: "Is it free?",
    text: "Yes. EVB IDE is open source under the GPL, version 3 or later, with no account, ads or subscription.",
  },
  {
    title: "How is it related to Zed and JetBrains?",
    text: "It is an independent fork of Zed with a JetBrains-style setup. It is not affiliated with Zed Industries or JetBrains.",
  },
  {
    title: "Can I keep Zed installed?",
    text: "Yes. EVB IDE is a separate app with its own command-line tool, settings and data folders.",
  },
  {
    title: "Which systems does it run on?",
    text: "Published builds are for macOS 12 or later on Apple Silicon, Linux x64 with glibc 2.35 or later, and Windows 10 or later on x64.",
  },
  {
    title: "Does it use AI or send telemetry?",
    text: "AI features, edit predictions and telemetry are off by default. AI features can be turned on in the settings.",
  },
  {
    title: "How does it update?",
    text: "It checks GitHub for new releases, downloads them in the background and installs them when you restart. Set auto_update to false in the settings to update by hand instead.",
  },
];

const releaseDate = new Intl.DateTimeFormat("en-US", { dateStyle: "medium", timeZone: "UTC" });

function formatDate(date: string) {
  return releaseDate.format(new Date(date));
}

function formatSize(size: number) {
  return `${Math.round(size / 1024 / 1024)} MB`;
}
</script>

<style scoped>
.hero {
  display: grid;
  grid-template-columns: minmax(300px, 400px) minmax(0, 1fr);
  gap: 48px;
  align-items: start;
  padding: 56px 0 72px;
}

.hero-title {
  font-size: clamp(36px, 4.2vw, 52px);
  font-weight: 400;
  text-wrap: balance;
  line-height: 1.08;
  letter-spacing: -1.6px;
}

.hero-lede {
  margin-top: 20px;
  color: var(--site-soft);
  font-size: 16px;
  line-height: 1.65;
}

.installer-card {
  margin-top: 28px;
  padding: 14px;
  border: 1px solid var(--site-border);
  border-radius: 12px;
  background: var(--site-surface);
}

.installer-version {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
  margin: 2px 2px 12px;
  color: var(--site-muted);
  font-size: 13px;
}

.installer-version-name {
  color: var(--site-ink);
  font-size: 15px;
  font-weight: 650;
}

.installer-platform {
  margin-bottom: 10px;
  color: var(--site-muted);
  font-family: var(--font-mono);
  font-size: 11px;
  letter-spacing: 1.5px;
  text-transform: uppercase;
}

.installer-tabs {
  display: grid;
  grid-auto-columns: 1fr;
  grid-auto-flow: column;
  gap: 4px;
  margin-bottom: 10px;
  padding: 3px;
  border: 1px solid var(--site-border);
  border-radius: 9px;
}

.installer-tab {
  padding: 7px 10px;
  border-radius: 6px;
  color: var(--site-soft);
  font-size: 13px;
}

.installer-tab[aria-selected="true"] {
  background: var(--site-ink);
  color: var(--site-background);
}

.installer-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 14px;
  border: 1px solid var(--site-border);
  border-radius: 9px;
  text-decoration: none;
}

.installer-row + .installer-row {
  margin-top: 8px;
}

.installer-row:hover {
  border-color: var(--site-accent);
}

.installer-row-recommended {
  background: var(--site-tint);
}

.installer-text {
  display: grid;
  gap: 3px;
}

.installer-name {
  display: flex;
  align-items: center;
  gap: 8px;
  font-weight: 600;
}

.installer-badge {
  padding: 2px 7px;
  border: 1px solid var(--site-accent);
  border-radius: 999px;
  color: var(--site-accent);
  font-size: 10px;
  font-weight: 600;
  letter-spacing: 0.6px;
  text-transform: uppercase;
}

.installer-meta {
  color: var(--site-muted);
  font-family: var(--font-mono);
  font-size: 12px;
}

.installer-icon {
  display: grid;
  place-items: center;
  width: 36px;
  height: 36px;
  border-radius: 8px;
  background: var(--site-accent);
  color: var(--site-on-accent);
}

.installer-note {
  display: grid;
  gap: 2px;
  margin: 12px 0 10px;
  color: var(--site-muted);
  font-size: 13px;
  line-height: 1.5;
  text-wrap: pretty;
}

.installer-requirement {
  color: var(--site-soft);
  font-weight: 550;
}

.installer-note code {
  font-family: var(--font-mono);
  font-size: 12px;
  white-space: nowrap;
}

.installer-links {
  display: flex;
  flex-wrap: wrap;
  gap: 16px;
  font-size: 13px;
}

.installer-links a {
  color: var(--site-accent);
}

.section {
  padding: 40px 0 72px;
}

.section-label {
  margin-bottom: 28px;
  color: var(--site-muted);
  font-family: var(--font-mono);
  font-size: 12px;
  font-weight: 400;
  letter-spacing: 3px;
  text-align: center;
  text-transform: uppercase;
}

.feature-grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  overflow: hidden;
  border: 1px solid var(--site-border);
  border-radius: 12px;
  background: var(--site-border);
  gap: 1px;
}

.feature-card {
  padding: 28px;
  background: var(--site-surface);
}

.feature-icon {
  width: 20px;
  height: 20px;
  margin-bottom: 18px;
  color: var(--site-accent);
}

.feature-title {
  margin-bottom: 10px;
  font-size: 16px;
  font-weight: 600;
}

.feature-text {
  color: var(--site-soft);
  font-size: 14px;
  line-height: 1.6;
}

.questions {
  max-width: 760px;
  margin: 0 auto;
}

.question {
  padding: 20px 0;
  border-bottom: 1px solid var(--site-border);
}

.question-title {
  margin-bottom: 8px;
  font-weight: 600;
}

.question-text {
  margin: 0;
  color: var(--site-soft);
  font-size: 15px;
  line-height: 1.6;
}

@media (max-width: 1000px) {
  .hero {
    grid-template-columns: 1fr;
    gap: 36px;
    padding-top: 40px;
  }

  .feature-grid {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}

@media (max-width: 640px) {
  .feature-grid {
    grid-template-columns: 1fr;
  }

  .feature-card {
    padding: 22px;
  }
}
</style>
