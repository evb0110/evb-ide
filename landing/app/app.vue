<template>
  <UApp>
    <div class="site-shell">
      <a class="skip-link" href="#main">Skip to content</a>
      <header class="site-header">
        <NuxtLink to="/" class="brand" aria-label="EVB IDE home">
          <img src="/icon.png" width="40" height="40" alt="" />
          <span><strong>EVB</strong> IDE<span class="brand-period">.</span></span>
        </NuxtLink>
        <nav aria-label="Main navigation" class="site-nav">
          <NuxtLink to="/#features">Features</NuxtLink>
          <NuxtLink to="/docs">Guide</NuxtLink>
          <UColorModeButton color="neutral" variant="ghost" />
          <UButton
            :to="repositoryUrl"
            icon="i-lucide-github"
            color="neutral"
            variant="ghost"
            aria-label="Source code on GitHub"
          />
        </nav>
      </header>
      <main id="main"><NuxtPage /></main>
      <footer class="site-footer">
        <div>
          <NuxtLink to="/" class="footer-brand">EVB IDE</NuxtLink>
          <p>
            An independent fork of <a href="https://zed.dev">Zed</a>.<br />Not affiliated with Zed Industries or
            JetBrains.
          </p>
        </div>
        <nav aria-label="Footer navigation">
          <NuxtLink to="/docs">Installation &amp; guide</NuxtLink>
          <NuxtLink to="/license">License &amp; credits</NuxtLink>
          <NuxtLink to="/privacy">Privacy</NuxtLink>
          <a :href="`${repositoryUrl}/issues`">Report an issue</a>
          <a href="https://evb-viewer.com">EVB Viewer ↗</a>
        </nav>
        <p class="copyright">
          © {{ year }} Eugene Barsky &amp; contributors<br />GPL-3.0-or-later
        </p>
      </footer>
    </div>
  </UApp>
</template>

<script setup lang="ts">
import { repositoryUrl } from "~~/shared/releases";

const year = new Date().getFullYear();
const config = useRuntimeConfig();
const route = useRoute();
useHead(() => ({ link: [{ rel: "canonical", href: new URL(route.path, config.public.siteUrl).href }] }));
useSeoMeta({
  description:
    "EVB IDE is the Zed editor with the JetBrains keymap, IntelliJ themes, run configurations, a Git Log tool window and GitHub pull request review. Free and open source.",
  ogSiteName: "EVB IDE",
  ogType: "website",
  ogImage: `${config.public.siteUrl}/films/ide/light-poster.jpg`,
  twitterCard: "summary_large_image",
});
</script>
