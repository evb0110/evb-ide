<template>
  <article class="reading-page">
    <h1>Privacy</h1>
    <p>
      EVB IDE starts with telemetry, AI features, edit predictions, and sign-in UI disabled. You do not need an account
      to edit local projects. Your settings can change these defaults.
    </p>
    <h2>This website</h2>
    <p>
      The site is hosted by Vercel. Requests include normal connection information, such as your IP address, browser
      headers, and requested page, which the hosting provider may process for delivery, security, and operational logs.
      See <a target="_blank" href="https://vercel.com/legal/privacy-policy">Vercel's privacy policy</a>.
    </p>
    <p>
      This website adds no analytics or advertising scripts. The appearance selector stores your light, dark, or system
      preference in your browser's local storage. Fonts and the app video are served by this site.
    </p>
    <p>
      The server requests public release information from GitHub. Following source or download links takes you to
      GitHub, which processes those requests under
      <a target="_blank" href="https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement"
        >its own privacy policy</a
      >.
    </p>
    <h2>The desktop app</h2>
    <p>
      Telemetry being off does not make the editor offline. Language extensions, language servers, package tools, Git
      remotes, and other enabled integrations can make network requests. Some components download when first used.
    </p>
    <p>
      If you enable AI, collaboration, or other hosted services, review the provider's terms and privacy policy. In
      particular, Zed-operated services are covered by
      <a target="_blank" href="https://zed.dev/privacy-policy">Zed's privacy policy</a> and service terms.
    </p>
    <h2>Questions</h2>
    <p>
      You can
      <a target="_blank" href="https://github.com/evb0110/evb-ide/issues">contact the project through GitHub</a>. Issues
      are public, so keep private code, credentials, and personal information out of reports.
    </p>
  </article>
</template>

<script setup lang="ts">
useSeoMeta({
  title: "Privacy · EVB IDE",
  description: "EVB IDE privacy defaults, website hosting, local preferences, and third-party services.",
});
</script>
