<template>
  <UApp>
    <div class="site-shell">
      <a class="skip-link" href="#main">Skip to content</a>
      <header class="site-header">
        <NuxtLink to="/" class="brand" aria-label="EVB IDE home">
          <BrandMark class="brand-mark" />
          <span><strong>EVB</strong> IDE</span>
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
        <div class="footer-main">
          <NuxtLink to="/" class="footer-brand"><strong>EVB</strong> IDE</NuxtLink>
          <nav aria-label="Footer navigation">
            <NuxtLink to="/docs">Installation &amp; guide</NuxtLink>
            <NuxtLink to="/license">License &amp; credits</NuxtLink>
            <NuxtLink to="/privacy">Privacy</NuxtLink>
            <a :href="`${repositoryUrl}/issues`">Report an issue</a>
          </nav>
        </div>
        <div class="footer-legal">
          <p>
            © {{ year }} <a href="https://evb-stack.com/" class="copyright-author">Eugene Barsky</a> &amp;
            contributors · GPL-3.0-or-later
          </p>
          <p>
            An independent fork of <a href="https://zed.dev">Zed</a>. Not affiliated with Zed Industries or
            JetBrains.
          </p>
        </div>
      </footer>
    </div>
  </UApp>
</template>

<script setup lang="ts">
import interFont from "~/assets/fonts/inter-latin-wght-normal.woff2?url";
import ideFilm from "~/films/ide.json";
import { repositoryUrl } from "~~/shared/releases";

useHead({
  link: [{ rel: "preload", href: interFont, as: "font", type: "font/woff2", crossorigin: "anonymous" }],
});

const year = new Date().getFullYear();
const config = useRuntimeConfig();
const route = useRoute();
useHead(() => ({ link: [{ rel: "canonical", href: new URL(route.path, config.public.siteUrl).href }] }));
useSeoMeta({
  description:
    "EVB IDE is the Zed editor with the JetBrains keymap, IntelliJ themes, run configurations, a Git Log tool window and GitHub pull request review. Free and open source.",
  ogSiteName: "EVB IDE",
  ogType: "website",
  ogImage: `${config.public.siteUrl}/films/ide/light-poster.jpg?v=${ideFilm.version}`,
  twitterCard: "summary_large_image",
});
</script>
