<template>
  <div>
    <section class="hero">
      <div class="hero-copy">
        <p class="eyebrow"><span class="status-dot" /> OPEN SOURCE · BUILT ON ZED</p>
        <h1>A familiar workflow.<br /><span>A fast editor.</span></h1>
        <p class="lede">
          JetBrains shortcuts. IntelliJ themes. Run configurations and Git tools where you expect them. The Zed editor,
          set up for your everyday web development.
        </p>
        <div class="hero-buttons">
          <UButton to="#download" size="xl" icon="i-lucide-download" class="primary-button">Get EVB IDE</UButton>
          <UButton
            :to="repositoryUrl"
            color="neutral"
            variant="outline"
            size="xl"
            trailing-icon="i-lucide-arrow-up-right"
            >Explore the source</UButton
          >
        </div>
        <p class="hero-note">Free to use, modify, and share. No account required.</p>
      </div>
      <div class="hero-side-note">
        <span>01 / THE EDITOR</span>
        <p>Your projects.<br />Your shortcuts.<br />A little more focus.</p>
      </div>
    </section>

    <section class="workspace-showcase" aria-label="EVB IDE screenshots">
      <div class="showcase-toolbar">
        <span class="showcase-label"><span class="status-dot" /> EVB IDE IN ACTION</span>
        <div class="screenshot-tabs" role="group" aria-label="Choose an app screenshot">
          <button
            v-for="(shot, index) in screenshots"
            :key="shot.file"
            :aria-pressed="activeScreenshot === index"
            @click="activeScreenshot = index"
          >
            {{ shot.label }}
          </button>
        </div>
      </div>
      <img
        v-if="currentScreenshot"
        :src="`/screenshots/${currentScreenshot.file}`"
        :alt="currentScreenshot.alt"
        width="1600"
        height="1000"
        fetchpriority="high"
        class="workspace-image"
      />
      <p class="showcase-caption">{{ currentScreenshot?.caption }} <span>Real app captures · macOS</span></p>
    </section>

    <section id="features" class="features-section">
      <div class="section-heading">
        <p class="eyebrow">02 / WHAT FEELS FAMILIAR</p>
        <h2>Less setup.<br />More of your workflow.</h2>
        <p>A focused set of changes on top of Zed. Keep its editing experience and add the habits you already know.</p>
      </div>
      <div class="feature-grid">
        <article v-for="feature in features" :key="feature.title" class="feature-card">
          <UIcon :name="feature.icon" class="feature-icon" />
          <h3>{{ feature.title }}</h3>
          <p>{{ feature.description }}</p>
          <span class="feature-detail">{{ feature.detail }}</span>
        </article>
      </div>
    </section>

    <section class="workflow-strip">
      <div>
        <span class="eyebrow">A FEW SHORTCUTS TO FEEL AT HOME</span>
        <h2>Already in your fingers.</h2>
      </div>
      <dl>
        <div>
          <dt>Run</dt>
          <dd><kbd>⌃ R</kbd></dd>
        </div>
        <div>
          <dt>Debug Node.js</dt>
          <dd><kbd>⌃ D</kbd></dd>
        </div>
        <div>
          <dt>Git Log</dt>
          <dd><kbd>⌘ 9</kbd></dd>
        </div>
        <div>
          <dt>Commit and Push</dt>
          <dd><kbd>⌥ ⌘ K</kbd></dd>
        </div>
      </dl>
    </section>

    <section id="download" class="download-section">
      <div class="section-heading">
        <p class="eyebrow">03 / MAKE YOURSELF AT HOME</p>
        <h2>Your next editor.<br />Ready when you are.</h2>
        <p>Independent from your Zed installation, with its own settings and application data.</p>
        <NuxtLink to="/docs" class="text-link"
          >Read the installation guide <UIcon name="i-lucide-arrow-right"
        /></NuxtLink>
      </div>
      <div class="download-card" aria-live="polite">
        <div class="download-heading">
          <img src="/icon.png" width="56" height="56" alt="" />
          <div>
            <h3>EVB IDE for macOS</h3>
            <p>{{ release?.tag || "Desktop downloads" }}</p>
          </div>
          <span class="license-badge">GPL v3+</span>
        </div>
        <template v-if="release">
          <a v-for="installer in release.installers" :key="installer.name" :href="installer.url" class="download-link"
            ><div>
              <strong>{{ installer.architecture === "arm64" ? "Apple Silicon" : "Intel Mac" }}</strong
              ><span>macOS 12 or later · DMG · {{ formatSize(installer.size) }}</span>
            </div>
            <UIcon name="i-lucide-download"
          /></a>
          <p class="download-help">
            Open the disk image, then drag EVB IDE into Applications. Updates are installed manually.
          </p>
          <div class="release-links">
            <a :href="release.url">Release notes</a><a :href="release.sourceUrl">Corresponding source</a
            ><a :href="release.checksumsUrl">SHA-256 checksums</a>
          </div>
        </template>
        <template v-else-if="error"
          ><p class="download-help">
            We couldn't load the latest download details. All published builds and their sources are available on
            GitHub.
          </p>
          <UButton :to="`${repositoryUrl}/releases`" icon="i-lucide-github" block size="lg"
            >View releases on GitHub</UButton
          ></template
        >
        <template v-else-if="status === 'pending'"><p class="download-help">Loading the latest release…</p></template>
        <template v-else
          ><p class="download-help">
            The first signed release is being prepared. You can explore the source and build EVB IDE today.
          </p>
          <UButton :to="`${repositoryUrl}/releases`" color="neutral" variant="outline" block size="lg"
            >View release status</UButton
          ></template
        >
        <p class="platform-note">
          Only published builds are listed here. For other platforms, see the
          <a :href="`${repositoryUrl}/blob/main/EVB.md`">source build guide</a>.
        </p>
      </div>
    </section>

    <section class="open-source-note">
      <UIcon name="i-lucide-code-xml" />
      <div>
        <h2>A fork, in the open.</h2>
        <p>
          EVB IDE builds on the work of Zed and its contributors. The full source, changes, and release build
          instructions are public. You can use it, change it, and make it your own under the GPL.
        </p>
        <NuxtLink to="/license" class="text-link"
          >Licensing and acknowledgements <UIcon name="i-lucide-arrow-right"
        /></NuxtLink>
      </div>
      <span class="source-mark">&lt;/&gt;</span>
    </section>
  </div>
</template>

<script setup lang="ts">
import { repositoryUrl } from "~~/shared/releases";

const { data, error, status } = await useFetch("/api/releases/latest");
const release = computed(() => data.value?.release);
const activeScreenshot = ref(0);
const screenshots = [
  {
    label: "Workspace",
    file: "workspace.png",
    alt: "EVB IDE with an IntelliJ light theme, TypeScript editor, project tree, and run configuration picker",
    caption: "The editor, with IntelliJ Light and the JetBrains keymap.",
  },
  {
    label: "Run configurations",
    file: "run.png",
    alt: "EVB IDE title-bar menu listing run configurations from package.json",
    caption: "Run project scripts directly from the title bar.",
  },
  {
    label: "Git Log",
    file: "git.png",
    alt: "EVB IDE Git Log tool window with a branches tree below the editor",
    caption: "Your branch history, alongside the code you are working on.",
  },
  {
    label: "Dark theme",
    file: "dark.png",
    alt: "EVB IDE using IntelliJ Dark with Vue source code open",
    caption: "IntelliJ Dark, JetBrains Mono, and a quieter workspace.",
  },
];
const currentScreenshot = computed(() => screenshots[activeScreenshot.value]);
const features = [
  {
    icon: "i-lucide-keyboard",
    title: "Familiar from the first file.",
    description:
      "JetBrains shortcuts, IntelliJ Light and Dark, file icons, JetBrains Mono, and Inter. Ready out of the box.",
    detail: "YOUR EDITOR, ALREADY SET UP",
  },
  {
    icon: "i-lucide-play",
    title: "Pick a task. Get it running.",
    description:
      "Package scripts, Composer scripts, and Zed tasks in a title-bar picker. Run, rerun, stop, or start a Node.js debug session.",
    detail: "NPM · PNPM · YARN · BUN · COMPOSER",
  },
  {
    icon: "i-lucide-git-branch",
    title: "Keep your history in view.",
    description:
      "A Git Log tool window with a branches tree, history filters, commit actions, compact blame, and Commit and Push.",
    detail: "CODE ABOVE. CONTEXT BELOW.",
  },
  {
    icon: "i-lucide-code-xml",
    title: "At home in a web project.",
    description:
      "Defaults for Vue, TypeScript, PHP, and Blade, with fixes to completion and rename handling across language servers.",
    detail: "BUILT FOR EVERYDAY WEB DEVELOPMENT",
  },
  {
    icon: "i-lucide-shield-check",
    title: "A quieter starting point.",
    description:
      "AI features, telemetry, and sign-in UI are off by default. No account is needed to edit local projects.",
    detail: "YOUR SETTINGS STAY IN YOUR HANDS",
  },
  {
    icon: "i-lucide-github",
    title: "A good neighbour to Zed.",
    description: "A separate app, CLI name, and settings directory. Keep Zed installed and try EVB IDE alongside it.",
    detail: "INDEPENDENT APP. SHARED ROOTS.",
  },
];
function formatSize(size: number) {
  return `${Math.round(size / 1024 / 1024)} MB`;
}
</script>
