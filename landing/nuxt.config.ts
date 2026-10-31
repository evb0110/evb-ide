export default defineNuxtConfig({
  compatibilityDate: "2026-10-01",
  devtools: { enabled: false },
  modules: ["@nuxt/ui"],
  css: ["~/assets/css/main.css"],
  ui: { fonts: false },
  runtimeConfig: {
    githubToken: "",
    public: { siteUrl: "https://evb-ide.vercel.app" },
  },
  typescript: { strict: true },
  icon: {
    clientBundle: {
      icons: [
        "lucide:github",
        "lucide:download",
        "lucide:sun",
        "lucide:moon",
        "lucide:play",
        "lucide:pause",
        "lucide:git-branch",
        "lucide:git-pull-request",
        "lucide:history",
        "lucide:code-xml",
        "lucide:keyboard",
      ],
    },
  },
  app: {
    head: {
      htmlAttrs: { lang: "en" },
      title: "EVB IDE: a JetBrains-style IDE built on Zed",
      meta: [{ name: "viewport", content: "width=device-width, initial-scale=1" }],
      link: [{ rel: "icon", type: "image/png", href: "/icon.png" }],
    },
  },
  routeRules: {
    "/**": {
      headers: {
        "X-Content-Type-Options": "nosniff",
        "X-Frame-Options": "DENY",
        "Referrer-Policy": "strict-origin-when-cross-origin",
        "Permissions-Policy": "camera=(), microphone=(), geolocation=()",
        "Content-Security-Policy":
          "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'",
      },
    },
    // Not fingerprinted, so cached for a day rather than for good.
    "/films/**": { headers: { "cache-control": "public, max-age=86400, stale-while-revalidate=604800" } },
    "/docs": { prerender: true },
    "/license": { prerender: true },
    "/privacy": { prerender: true },
  },
});
