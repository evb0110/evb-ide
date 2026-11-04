<template>
  <figure class="film">
    <div class="film-window">
      <div class="film-titlebar" aria-hidden="true">
        <span class="film-light film-light-close" />
        <span class="film-light film-light-min" />
        <span class="film-light film-light-max" />
        <span class="film-title">{{ title }}</span>
      </div>
      <div class="film-screen" :style="{ aspectRatio: `${film.width} / ${film.height}` }">
        <video
          v-if="mounted"
          :key="theme"
          ref="video"
          class="film-video"
          :src="filmUrl(`${theme}.mp4`)"
          :aria-label="label"
          muted
          playsinline
          loop
          preload="auto"
          disablepictureinpicture
          @loadedmetadata="onMetadata"
          @playing="showPoster = false"
          @play="playing = true"
          @pause="playing = false"
        />
        <!-- The first frame, or the reduced-motion still, until the video shows it. CSS picks the
             theme already set on the html element, so the server render matches. -->
        <div v-if="showPoster" class="film-poster" :style="posterStyle" />
      </div>
    </div>
    <figcaption class="film-controls">
      <button class="film-toggle" type="button" :aria-label="playing ? 'Pause' : 'Play'" @click="toggle">
        <UIcon :name="playing ? 'i-lucide-pause' : 'i-lucide-play'" />
      </button>
      <div
        class="film-track"
        role="slider"
        tabindex="0"
        aria-label="Position in the recording"
        :aria-valuemin="0"
        :aria-valuemax="Math.round(duration)"
        :aria-valuenow="Math.round(time)"
        @pointerdown="startScrub"
        @pointermove="moveScrub"
        @lostpointercapture="endScrub"
        @keydown.left.prevent="seekTo(time - 1)"
        @keydown.right.prevent="seekTo(time + 1)"
      >
        <div class="film-fill" :style="{ width: `${(time / Math.max(duration, 0.001)) * 100}%` }" />
      </div>
      <span class="film-time">{{ time.toFixed(1) }}s</span>
    </figcaption>
  </figure>
</template>

<script setup lang="ts">
import { useIntersectionObserver, usePreferredReducedMotion, useRafFn } from "@vueuse/core";
import ideFilm from "~/films/ide.json";

type TFilmTheme = "light" | "dark";

interface IFilmIndex {
  fps: number;
  /** First frame after the opening fade: the poster, and where playback starts. */
  intro: number;
  width: number;
  height: number;
  /** Content hash of the rendered files, appended to their URLs. */
  version: string;
  variants: Record<string, { frames: number; still: number }>;
}

const { theme, label, title } = defineProps<{
  theme: TFilmTheme;
  label: string;
  title: string;
}>();

// Rendered by recorder/render.mjs: one video per theme, with a poster and a still.
const FILM_BASE = "/films/ide";
const film: IFilmIndex = ideFilm;
const filmUrl = (file: string) => `${FILM_BASE}/${file}?v=${film.version}`;
const video = useTemplateRef<HTMLVideoElement>("video");
const reducedMotion = usePreferredReducedMotion();
const mounted = ref(false);
const playing = ref(false);
const showPoster = ref(true);
const visible = ref(false);
const time = ref(0);
const duration = ref(0);

const still = computed(() => reducedMotion.value === "reduce");

// Turning on Reduce Motion while the film plays stops it on its still.
watch(still, (isStill) => {
  const element = video.value;
  const info = film.variants[theme];
  if (!isStill || !element) {
    return;
  }
  element.pause();
  if (info && element.readyState >= 1) {
    element.currentTime = info.still / film.fps;
    time.value = element.currentTime;
  }
});

// The poster is the film's first paint; fetch the one for the visitor's color scheme early.
useHead({
  link: (["light", "dark"] as const).map((scheme) => ({
    rel: "preload",
    as: "image",
    href: filmUrl(`${scheme}-poster.jpg`),
    media: `(prefers-color-scheme: ${scheme})`,
  })),
});
const posterStyle = computed(() => {
  const kind = still.value ? "still" : "poster";
  return {
    "--poster-light": `url(${filmUrl(`light-${kind}.jpg`)})`,
    "--poster-dark": `url(${filmUrl(`dark-${kind}.jpg`)})`,
  };
});

// The visitor's Pause holds until they press Play: scrolling back or switching the theme
// must not start the film again.
const pausedByVisitor = ref(false);

function play() {
  showPoster.value = false;
  void video.value?.play().catch(() => {});
}

function onMetadata() {
  const element = video.value;
  const info = film.variants[theme];
  if (!element || !info) {
    return;
  }
  duration.value = element.duration;
  element.currentTime = (still.value ? info.still : film.intro) / film.fps;
  time.value = element.currentTime;
  if (visible.value && !still.value && !pausedByVisitor.value) {
    play();
  }
}

function toggle() {
  if (video.value?.paused) {
    pausedByVisitor.value = false;
    play();
  } else {
    pausedByVisitor.value = true;
    video.value?.pause();
  }
}

function seekTo(seconds: number) {
  const element = video.value;
  if (!element || !duration.value) {
    return;
  }
  element.currentTime = Math.min(duration.value, Math.max(0, seconds));
  time.value = element.currentTime;
  showPoster.value = false;
}

// A scrub belongs to the pointer that started it; capture keeps its moves on the track.
let scrubPointerId: number | null = null;

function seekFromPointer(event: PointerEvent) {
  if (!(event.currentTarget instanceof HTMLElement)) {
    return;
  }
  const rect = event.currentTarget.getBoundingClientRect();
  seekTo(Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width)) * duration.value);
}

function startScrub(event: PointerEvent) {
  if (!(event.currentTarget instanceof HTMLElement)) {
    return;
  }
  scrubPointerId = event.pointerId;
  event.currentTarget.setPointerCapture(event.pointerId);
  seekFromPointer(event);
}

function moveScrub(event: PointerEvent) {
  if (event.pointerId === scrubPointerId) {
    seekFromPointer(event);
  }
}

function endScrub() {
  scrubPointerId = null;
}

// timeupdate fires only a few times a second; follow the playhead every frame instead.
useRafFn(() => {
  if (video.value && playing.value) {
    time.value = video.value.currentTime;
  }
});

// The other theme loads the other recording; its poster shows until it plays.
watch(
  () => theme,
  () => {
    playing.value = false;
    showPoster.value = true;
    time.value = 0;
  },
);

// Plays only while on screen.
useIntersectionObserver(video, ([entry]) => {
  visible.value = Boolean(entry?.isIntersecting);
  const element = video.value;
  if (!element || still.value || element.readyState < 1) {
    return;
  }
  if (visible.value) {
    if (!pausedByVisitor.value) {
      play();
    }
  } else {
    element.pause();
  }
});

onMounted(() => {
  // The color mode and the reduced-motion preference are known only in the browser.
  mounted.value = true;
});
</script>

<style scoped>
.film {
  margin: 0;
}

.film-window {
  overflow: hidden;
  border: 1px solid var(--site-border);
  border-radius: 12px;
  background: #fff;
  box-shadow:
    0 1px 2px rgb(28 26 22 / 8%),
    0 24px 60px -18px rgb(28 26 22 / 38%);
}

.film-titlebar {
  position: relative;
  display: flex;
  align-items: center;
  gap: 8px;
  height: 34px;
  padding: 0 14px;
  border-bottom: 1px solid rgb(24 24 27 / 10%);
  background: #eceef0;
}

.film-light {
  width: 12px;
  height: 12px;
  border-radius: 50%;
}

.film-light-close {
  background: #ff5f57;
}

.film-light-min {
  background: #febc2e;
}

.film-light-max {
  background: #28c840;
}

.film-title {
  position: absolute;
  left: 50%;
  color: #52525b;
  font-size: 12px;
  font-weight: 600;
  transform: translateX(-50%);
}

.film-screen {
  position: relative;
  width: 100%;
  background: #fff;
}

:global(html.dark .film-window),
:global(html.dark .film-screen) {
  background: #1e1f22;
}

:global(html.dark .film-titlebar) {
  border-bottom-color: rgb(255 255 255 / 6%);
  background: #2b2d30;
}

:global(html.dark .film-title) {
  color: #a1a1aa;
}

.film-video,
.film-poster {
  position: absolute;
  inset: 0;
  display: block;
  width: 100%;
  height: 100%;
}

.film-poster {
  background: var(--poster-light) center / cover no-repeat;
}

:global(html.dark .film-poster) {
  background-image: var(--poster-dark);
}

.film-controls {
  display: flex;
  align-items: center;
  gap: 14px;
  margin-top: 14px;
  padding: 0 4px;
}

.film-toggle {
  display: grid;
  flex: none;
  place-items: center;
  width: 30px;
  height: 30px;
  border-radius: 8px;
  color: var(--site-ink);
}

.film-toggle:hover,
.film-toggle:focus-visible {
  background: var(--site-surface);
}

.film-track {
  position: relative;
  flex: 1;
  height: 4px;
  overflow: hidden;
  border-radius: 2px;
  background: var(--site-border);
  cursor: pointer;
  /* Horizontal drags scrub; a vertical drag pans the page and ends the scrub. */
  touch-action: pan-y;
}

.film-fill {
  height: 100%;
  background: var(--site-accent);
}

.film-time {
  min-width: 44px;
  color: var(--site-muted);
  font-family: var(--font-mono);
  font-size: 12px;
  text-align: right;
}
</style>
