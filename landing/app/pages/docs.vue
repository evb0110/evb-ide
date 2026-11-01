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
    <h2>Install on Linux</h2>
    <ol>
      <li><NuxtLink to="/#download">Download the Linux archive</NuxtLink> (x64).</li>
      <li>Extract it and run <code>./evb-ide/install.sh</code>.</li>
      <li>Run <code>evb-ide</code>, or open EVB IDE from your applications menu.</li>
    </ol>
    <p>
      The installer copies EVB IDE to <code>~/.local/evb-ide</code>, links the <code>evb-ide</code> command into
      <code>~/.local/bin</code> and adds a menu entry, all for your user. The build needs glibc 2.35 or later (Ubuntu
      22.04, Debian 12, Fedora 36 or newer) and a GPU with Vulkan support.
    </p>
    <h2>Install on Windows</h2>
    <ol>
      <li><NuxtLink to="/#download">Download the Windows archive</NuxtLink> (x64).</li>
      <li>Extract it to a folder you keep, such as <code>%LOCALAPPDATA%\Programs</code>.</li>
      <li>Run <code>evb-ide.exe</code> in the extracted <strong>EVB IDE</strong> folder.</li>
    </ol>
    <p>
      The archive is portable: it needs no installer and runs on Windows 10 or later. The command line tool is
      <code>bin\evb-ide.exe</code>; add the <code>bin</code> folder to your <code>PATH</code> to use it from a terminal.
      The Windows build is not code-signed, so SmartScreen may ask before it first runs.
    </p>
    <h2>Your first project</h2>
    <p>
      Open a folder containing your project. The run picker discovers package.json scripts, Composer scripts, and
      <code>.zed/tasks.json</code> tasks. Install your project's dependencies using its usual package manager first.
      Language extensions and servers may download on first use.
    </p>
    <p>
      Open more projects the same way, or from <strong>File › Open Recent</strong>. Each project gets its own window. On
      macOS, project windows are tabs of one window, like WebStorm. Opening a project that is already open switches to
      it.
    </p>
    <table>
      <thead>
        <tr>
          <th>Action</th>
          <th>macOS</th>
          <th>Linux and Windows</th>
        </tr>
      </thead>
      <tbody>
        <tr>
          <td>Run selected configuration</td>
          <td data-label="macOS"><kbd>⌃ R</kbd></td>
          <td data-label="macOS"><kbd>Shift F10</kbd></td>
        </tr>
        <tr>
          <td>Debug a Node.js configuration</td>
          <td data-label="macOS"><kbd>⌃ D</kbd></td>
          <td data-label="macOS"><kbd>Shift F9</kbd></td>
        </tr>
        <tr>
          <td>Stop</td>
          <td data-label="macOS"><kbd>⌘ F2</kbd></td>
          <td data-label="macOS"><kbd>Ctrl F2</kbd></td>
        </tr>
        <tr>
          <td>Choose run configuration</td>
          <td data-label="macOS"><kbd>⌃ ⌥ R</kbd></td>
          <td data-label="macOS"><kbd>Alt Shift F10</kbd></td>
        </tr>
        <tr>
          <td>Commit and Push</td>
          <td data-label="macOS"><kbd>⌥ ⌘ K</kbd></td>
          <td data-label="macOS"><kbd>Ctrl Alt K</kbd></td>
        </tr>
        <tr>
          <td>Git Log</td>
          <td data-label="macOS"><kbd>⌘ 9</kbd></td>
          <td data-label="macOS"><kbd>Alt 9</kbd></td>
        </tr>
        <tr>
          <td>Toggle bottom dock</td>
          <td data-label="macOS"><kbd>⌘ J</kbd></td>
          <td data-label="macOS"><kbd>Ctrl J</kbd></td>
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
      Download the next version from <a target="_blank" :href="`${repositoryUrl}/releases`">GitHub Releases</a>, quit EVB IDE, and
      replace it in Applications, run the new archive's <code>install.sh</code> on Linux, or replace the extracted folder on
      Windows. Your settings and application data remain in their own folders. Automatic upstream Zed
      updates are disabled.
    </p>
    <h2>Build from source</h2>
    <p>
      On macOS you need Xcode and its Metal toolchain, Rust through rustup, CMake, and the prerequisites in the
      <a target="_blank" :href="`${repositoryUrl}/blob/main/EVB.md`">development guide</a>. On Linux, <code>script/linux</code> installs
      the libraries and clang; the build needs clang 17 or newer. On Windows you need Visual Studio's C++ build tools,
      Rust through rustup and CMake.
    </p>
    <pre><code>git clone https://github.com/evb0110/evb-ide.git
cd evb-ide
script/evb-install -n       # macOS: target/evb-bundle/EVB IDE.app
script/evb-bundle-linux     # Linux: target/evb-bundle/EVB-IDE-VERSION-linux-x64.tar.gz
script/evb-bundle-windows.ps1   # Windows: target/evb-bundle/EVB-IDE-VERSION-windows-x64.zip</code></pre>
    <p>
      The macOS app gets an ad-hoc signature for local development; public releases use Developer ID signing and Apple
      notarization. Each release provides an archive with dependency sources and instructions for rebuilding that
      version.
    </p>
    <h2>Need help?</h2>
    <p>
      <a target="_blank" :href="`${repositoryUrl}/issues`">Open an issue</a> with your EVB IDE version, operating system, and steps to
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
