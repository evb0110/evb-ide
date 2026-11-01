<template>
  <article class="reading-page">
    <h1>License and credits</h1>
    <p>
      EVB IDE is an independent fork of <a target="_blank" href="https://github.com/zed-industries/zed">Zed</a>,
      originally developed by Zed Industries and its contributors. The fork is maintained by Eugene Barsky and
      contributors.
    </p>
    <h2>License</h2>
    <p>
      The editor is licensed primarily under
      <a target="_blank" :href="`${repositoryUrl}/blob/main/LICENSE-GPL`">GNU GPL version 3 or later</a>. Components
      marked <a target="_blank" :href="`${repositoryUrl}/blob/main/LICENSE-APACHE`">Apache 2.0</a>, including GPUI and
      the extension API, retain those terms. You may use, modify, and redistribute the software, including commercially,
      subject to the applicable licenses.
    </p>
    <p>
      When distributing a modified editor, preserve required notices, identify your changes, and provide the
      corresponding source under the GPL. The software comes without warranty, as described in the license texts.
    </p>
    <h2>Source for your download</h2>
    <p>
      Every <a target="_blank" :href="`${repositoryUrl}/releases`">published release</a> links the exact source tag, a
      source archive with dependency sources, build instructions, and SHA-256 checksums. The source link is listed
      beside the installer on the home page.
    </p>
    <h2>Credits and assets</h2>
    <ul>
      <li>
        IntelliJ Light and Dark themes and JetBrains file icons come from
        <a target="_blank" href="https://github.com/JetBrains/intellij-community">IntelliJ IDEA Community Edition</a>,
        under Apache 2.0.
      </li>
      <li>JetBrains Mono, Inter, Lilex, and IBM Plex fonts carry their original font licenses.</li>
      <li>Lucide and Feather icons carry their original ISC/MIT notices.</li>
      <li>The EVB IDE application icon is an original asset distributed under GPL-3.0-or-later.</li>
    </ul>
    <p>
      See the complete <a target="_blank" :href="`${repositoryUrl}/blob/main/NOTICE.md`">project notices</a>. Inside the
      app, <strong>Help → View Dependency Licenses</strong> displays dependency and asset license texts. Release bundles
      also carry notices in <code>Contents/Resources/licenses</code>.
    </p>
    <h2>Independent branding</h2>
    <p>
      EVB IDE is not affiliated with or endorsed by Zed Industries or JetBrains. Zed, JetBrains, IntelliJ, and WebStorm
      are trademarks of their respective owners. References identify the upstream project, asset sources, and familiar
      workflows.
    </p>
    <h2>Third-party services</h2>
    <p>
      The open-source licenses do not include subscriptions to hosted services. Zed-operated services, if enabled, are
      governed by <a target="_blank" href="https://zed.dev/terms">Zed's service terms</a>. Language extensions and
      servers may have separate licenses and terms.
    </p>
  </article>
</template>

<script setup lang="ts">
import { repositoryUrl } from "~~/shared/releases";
useSeoMeta({
  title: "License & credits · EVB IDE",
  description:
    "GPL licensing, corresponding source code, third-party credits, and independent branding for the EVB IDE fork of Zed.",
});
</script>
