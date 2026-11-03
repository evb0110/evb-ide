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
    <p id="command-line">
      To open projects from a terminal, choose <strong>EVB IDE › Install CLI</strong>. It installs the
      <code>evb</code> command in <code>/usr/local/bin</code>, so <code>evb path/to/project</code> opens a project. If
      that fails, link it yourself:
      <code>ln -s "/Applications/EVB IDE.app/Contents/MacOS/cli" /usr/local/bin/evb</code>.
    </p>
    <h2>Install on Linux</h2>
    <ol>
      <li><NuxtLink to="/#download">Download the Linux archive</NuxtLink> (x64).</li>
      <li>Extract it and run <code>./evb-ide/install.sh</code>.</li>
      <li>Open EVB IDE from your applications menu, or run <code>~/.local/bin/evb-ide</code>.</li>
    </ol>
    <p>
      The installer copies EVB IDE to <code>~/.local/evb-ide</code>, links the <code>evb-ide</code> command into
      <code>~/.local/bin</code> and adds a menu entry, all for your user. Add <code>~/.local/bin</code> to your
      <code>PATH</code> to run it as <code>evb-ide</code>. The build needs glibc 2.35 or later (Ubuntu 22.04, Debian 12,
      Fedora 36 or newer) and a GPU with Vulkan support.
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
      Open a folder containing your project. The run configuration picker in the title bar lists the scripts in each
      local project folder's <code>package.json</code> and <code>composer.json</code>, and your Zed tasks from
      <code>.zed/tasks.json</code> and the global tasks file. Scripts run with npm, pnpm, yarn or bun as the lockfile or
      <code>packageManager</code> says, including the enclosing workspace's for a package opened on its own, and npm
      otherwise. The picker remembers your choice. Run, Rerun and Stop work for every configuration; Debug starts the
      Node.js debugger for npm, pnpm and yarn scripts. Install your project's dependencies using its usual package
      manager first. Language extensions and servers may download on first use.
    </p>
    <p>
      Open more projects the same way, or from <strong>File › Open Recent</strong>. Each project gets its own window, and
      opening a project that is already open switches to it, also from the command line. On macOS, project windows are
      tabs of one window, like WebStorm: tabs show the project's name, with its parent folder when two projects share
      a name, and its folder on hover. Drag tabs to reorder them, use <strong>+</strong> to open a recent project in a
      new tab, and drag the empty part of the tab bar to move the window.
    </p>
    <p>
      The Project tree does not follow the active file by default. Click its <strong>Select Opened File</strong>
      button to show the file, or set <code>project_panel.auto_reveal_entries</code> to <code>true</code>. Click a part of
      the path above the editor to show it in the tree; right-click it to copy the file's full path.
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
          <td data-label="Linux and Windows"><kbd>Shift F10</kbd></td>
        </tr>
        <tr>
          <td>Debug a Node.js configuration</td>
          <td data-label="macOS"><kbd>⌃ D</kbd></td>
          <td data-label="Linux and Windows"><kbd>Shift F9</kbd></td>
        </tr>
        <tr>
          <td>Stop</td>
          <td data-label="macOS"><kbd>⌘ F2</kbd></td>
          <td data-label="Linux and Windows"><kbd>Ctrl F2</kbd></td>
        </tr>
        <tr>
          <td>Choose run configuration</td>
          <td data-label="macOS"><kbd>⌃ ⌥ R</kbd></td>
          <td data-label="Linux and Windows"><kbd>Alt Shift F10</kbd></td>
        </tr>
        <tr>
          <td>Commit and Push, in the Commit panel</td>
          <td data-label="macOS"><kbd>⌥ ⌘ K</kbd></td>
          <td data-label="Linux and Windows"><kbd>Ctrl Alt K</kbd></td>
        </tr>
        <tr>
          <td>Git Log</td>
          <td data-label="macOS"><kbd>⌘ 9</kbd></td>
          <td data-label="Linux and Windows"><kbd>Alt 9</kbd></td>
        </tr>
        <tr>
          <td>Toggle bottom dock</td>
          <td data-label="macOS"><kbd>⌘ J</kbd></td>
          <td data-label="Linux and Windows"><kbd>Ctrl J</kbd></td>
        </tr>
      </tbody>
    </table>
    <p>In the macOS terminal, <kbd>⌃ R</kbd> and <kbd>⌃ D</kbd> keep their usual meaning for the shell.</p>
    <h2>Git</h2>
    <p>
      The Commit panel's split button has <strong>Commit and Push</strong>. The Git Log opens in the bottom dock
      (<kbd>⌘ 9</kbd> or <kbd>Alt 9</kbd>). Click HEAD or a branch in the tree beside the log to show its history, and
      narrow it with the User, Date and Paths filters, which work in local projects and are remembered per repository.
      File histories open as editor tabs.
    </p>
    <p>
      Right-click a commit in the log or a file's history for WebStorm's commit menu: create a patch, cherry-pick,
      check out, reset (soft, mixed, hard or keep), revert, undo, edit the message, fix up with staged changes, drop,
      push up to it, work with the branches at it, and create a branch or tag. Changes to history need a history without
      merges after the commit, and ask first when the commits are already pushed.
    </p>
    <p>
      Right-click the line numbers and choose <strong>Annotate with Git Blame</strong> for a compact blame column with
      the date and the author's last name, like WebStorm's annotations. Read-only views of a file at an earlier commit
      have <strong>Open Working Copy</strong>, and go to definition there jumps to the matching code in your current
      files.
    </p>
    <h2>GitHub pull requests</h2>
    <p>
      For a branch on GitHub, the status bar shows its pull request: number, draft, merged or closed state, checks,
      review decision and conflicts. Its menu opens or copies the link, lists the checks and the review threads, and has
      <strong>Submit Review…</strong>. The status refreshes after commits and pushes and on its own, every 30 seconds
      while checks run. Sign in with <code>gh auth login</code>, or set <code>GH_TOKEN</code>; set
      <code>status_bar.pull_request_button</code> to <code>false</code> to hide it.
    </p>
    <p>
      <strong>Create Pull Request</strong> opens a form filled from your commits, like <code>gh pr create --fill</code>;
      the Commit panel's split button also has <strong>Commit, Push and Create Pull Request</strong> for a GitHub branch
      without an open pull request. Review threads show under their lines in the editor, with Reply, Resolve and Open on GitHub. Choose
      <strong>Add Pull Request Comment…</strong> in the line numbers' menu to comment on a line of the pull request's
      changes; lines you changed locally stay in the status bar's list.
    </p>
    <h2>Settings and defaults</h2>
    <p>
      Your settings file overrides the bundled defaults. Existing Zed settings are separate.
    </p>
    <ul>
      <li>macOS: <code>~/.config/evb-ide/settings.json</code>; app data in <code>~/Library/Application Support/EVB-IDE</code>.</li>
      <li>Linux: <code>~/.config/evb-ide/settings.json</code>; app data in <code>~/.local/share/evb-ide</code> (or the XDG directories you set).</li>
      <li>Windows: <code>%APPDATA%\EVB-IDE\settings.json</code>; app data in <code>%LOCALAPPDATA%\EVB-IDE</code>.</li>
    </ul>
    <p>
      The app follows your system's light or dark appearance using IntelliJ Light and Dark. It starts with the JetBrains
      keymap, JetBrains file icons, JetBrains Mono 13 without ligatures for code, and Inter 13 for the interface, with the
      Project and Commit panels on the left. AI features, telemetry and the sign-in button are off by default.
    </p>
    <p>
      The HTML, Vue, SCSS, PHP, Blade and Stylelint extensions install automatically; SCSS covers Vue's
      <code>&lt;style lang="scss"&gt;</code> blocks too. PHP and Blade use Intelephense, whose free version cannot rename;
      its premium features need a separate Intelephense license. Using Zed-operated services, if enabled, is subject to
      Zed's own terms.
    </p>
    <h2>Updates</h2>
    <p>
      EVB IDE checks this project's <a target="_blank" :href="`${repositoryUrl}/releases`">GitHub Releases</a> when it
      starts and every hour, downloads a newer version in the background and installs it when you restart. Choose
      <strong>Check for Updates</strong> to check now, or set <code>auto_update</code> to <code>false</code> to update by
      hand: download the next version, quit EVB IDE, and replace it in Applications, run the new archive's
      <code>install.sh</code> on Linux, or replace the extracted folder on Windows. Your settings and application data
      remain in their own folders.
    </p>
    <h2>Build from source</h2>
    <p>
      On macOS you need Xcode and its Metal toolchain, Rust through rustup, CMake, and the prerequisites in the
      <a target="_blank" :href="`${repositoryUrl}/blob/main/EVB.md`">development guide</a>. On Linux, <code>script/linux</code> installs
      the libraries and clang; the build needs clang 17 or newer. On Windows you need PowerShell 7, Visual Studio's C++
      build tools, Rust through rustup and CMake.
    </p>
    <pre><code>git clone https://github.com/evb0110/evb-ide.git
cd evb-ide
script/evb-install -n                     # macOS: target/evb-bundle/EVB IDE.app
script/evb-bundle-linux                   # Linux: target/evb-bundle/EVB-IDE-VERSION-linux-x64.tar.gz
pwsh -File script/evb-bundle-windows.ps1  # Windows: target/evb-bundle/EVB-IDE-VERSION-windows-x64.zip</code></pre>
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
