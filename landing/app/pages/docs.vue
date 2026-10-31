<template>
  <article class="reading-page">
    <h1>Install and get started</h1>
    <p>
      EVB IDE is a Zed-based editor with a JetBrains-style setup. It has its own app identity and settings, so you can
      try it alongside Zed.
    </p>
    <h2>Install on macOS</h2>
    <ol>
      <li>
        <NuxtLink to="/#download">Download the build</NuxtLink> for your Mac. Apple Silicon means M1 or later; Intel
        requires a separately listed Intel build.
      </li>
      <li>Open the disk image and drag <strong>EVB IDE</strong> into <strong>Applications</strong>.</li>
      <li>Launch EVB IDE from Applications and open a project folder.</li>
    </ol>
    <p>
      Published Mac builds require macOS 12 or later. Downloads include license notices; each release links its
      corresponding source and SHA-256 checksums. Only architectures with a published build are offered.
    </p>
    <h2>Your first project</h2>
    <p>
      Open a folder containing your project. The run picker discovers package.json scripts, Composer scripts, and
      <code>.zed/tasks.json</code> tasks. Install your project's dependencies using its usual package manager first.
      Language extensions and servers may download on first use.
    </p>
    <table>
      <thead>
        <tr>
          <th>Action</th>
          <th>macOS shortcut</th>
        </tr>
      </thead>
      <tbody>
        <tr>
          <td>Run selected configuration</td>
          <td><kbd>⌃ R</kbd></td>
        </tr>
        <tr>
          <td>Debug a Node.js configuration</td>
          <td><kbd>⌃ D</kbd></td>
        </tr>
        <tr>
          <td>Stop</td>
          <td><kbd>⌘ F2</kbd></td>
        </tr>
        <tr>
          <td>Choose run configuration</td>
          <td><kbd>⌃ ⌥ R</kbd></td>
        </tr>
        <tr>
          <td>Commit and Push</td>
          <td><kbd>⌥ ⌘ K</kbd></td>
        </tr>
        <tr>
          <td>Git Log</td>
          <td><kbd>⌘ 9</kbd></td>
        </tr>
        <tr>
          <td>Toggle bottom dock</td>
          <td><kbd>⌘ J</kbd></td>
        </tr>
      </tbody>
    </table>
    <h2>Settings and defaults</h2>
    <p>
      Edit <code>~/.config/evb-ide/settings.json</code> to override the bundled defaults. App data is stored in
      <code>~/Library/Application Support/EVB-IDE</code>. Existing Zed settings are separate.
    </p>
    <p>
      The app follows your system's light or dark appearance using IntelliJ Light and Dark. It starts with the JetBrains
      keymap, JetBrains file icons, JetBrains Mono, and Inter. AI features, telemetry, sign-in UI, and automatic updates
      are disabled by default.
    </p>
    <p>
      PHP and Blade use Intelephense. Its premium features need a separate Intelephense license. Using Zed-operated
      services, if enabled, is subject to Zed's own terms.
    </p>
    <h2>Updates</h2>
    <p>
      Download the next version from <a :href="`${repositoryUrl}/releases`">GitHub Releases</a>, quit EVB IDE, and
      replace it in Applications. Your settings and application data remain in their own folders. Automatic upstream Zed
      updates are disabled.
    </p>
    <h2>Build from source</h2>
    <p>
      The supported packaging script currently builds macOS. You need Xcode and its Metal toolchain, Rust through
      rustup, CMake, and the prerequisites in the <a :href="`${repositoryUrl}/blob/main/EVB.md`">development guide</a>.
    </p>
    <pre><code>git clone https://github.com/evb0110/evb-ide.git
cd evb-ide
script/evb-install -n</code></pre>
    <p>
      This produces <code>target/evb-bundle/EVB IDE.app</code> with an ad-hoc signature for local development. Public
      releases use Developer ID signing and Apple notarization. Each release provides an archive with dependency sources
      and instructions for rebuilding that version.
    </p>
    <h2>Need help?</h2>
    <p>
      <a :href="`${repositoryUrl}/issues`">Open an issue</a> with your EVB IDE version, macOS version, and steps to
      reproduce. EVB IDE is an independent community project; please report fork-specific issues here.
    </p>
  </article>
</template>

<script setup lang="ts">
import { repositoryUrl } from "~~/shared/releases";
useSeoMeta({
  title: "Installation & guide · EVB IDE",
  description:
    "Install EVB IDE, learn its shortcuts, configure your workspace, and build the open-source editor from source.",
});
</script>
