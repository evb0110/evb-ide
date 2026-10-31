> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# EVB IDE

A Zed-based editor with familiar JetBrains shortcuts, IntelliJ themes, and a
WebStorm-style workflow for web development.

[Website](https://evb-ide.vercel.app) ·
[Downloads](https://github.com/evb0110/evb-ide/releases) ·
[Build and development guide](EVB.md) ·
[Report an issue](https://github.com/evb0110/evb-ide/issues)

EVB IDE is an independent, community-maintained fork of
[Zed](https://github.com/zed-industries/zed). It is not affiliated with or
endorsed by Zed Industries or JetBrains.

## What changes

- IntelliJ Light and Dark themes, JetBrains file icons, JetBrains Mono, and
  the JetBrains keymap, configured out of the box.
- A title-bar run configuration picker for package.json scripts, Composer
  scripts, and Zed tasks, with Run, Debug for Node.js, Rerun, and Stop.
- A Git Log tool window with a branches tree, filters, commit operations,
  compact blame annotations, and Commit and Push.
- Vue, TypeScript, PHP, and Blade defaults, including fixes to language-server
  completion and rename behavior.
- Separate application identity and settings, so EVB IDE can run alongside Zed.
- AI features, telemetry, sign-in UI, and automatic updates disabled by default.

See [EVB.md](EVB.md) for the detailed differences and development checks.

## Install

Download a build for your Mac from [Releases](https://github.com/evb0110/evb-ide/releases).
Open the disk image and drag **EVB IDE** into **Applications**. Each release
lists its supported architecture and macOS requirements. Only published,
verified platforms are offered on the website.

Updates are manual: download the next release and replace the application.
Your EVB IDE settings and projects are preserved. The fork never replaces an
installed Zed with an upstream update.

To build from source, follow [EVB.md](EVB.md). The source tag and source archive
for each downloadable release are linked on its release page, together with
SHA-256 checksums and build instructions.

## Settings and services

On macOS, settings live in `~/.config/evb-ide/settings.json` and application
data lives in `~/Library/Application Support/EVB-IDE`. Your settings override
the bundled defaults. Language extensions and language servers can download
their own components and may have additional terms. PHP defaults use
Intelephense; its premium features require a separate license.

Using Zed-operated services, if you enable them, is subject to
[Zed's service terms](https://zed.dev/terms). EVB IDE does not include a Zed
subscription or a JetBrains product license.

## License and attribution

The editor is **GPL-3.0-or-later**, with **Apache-2.0** components where marked,
including GPUI. Existing component licenses remain in effect. You may use,
modify, and redistribute this fork under those terms, including commercially.

Read [LICENSE-GPL](LICENSE-GPL), [LICENSE-APACHE](LICENSE-APACHE), and
[NOTICE.md](NOTICE.md). The application's **Help → View Dependency Licenses**
includes bundled dependency and asset notices. The IntelliJ-derived themes
and icons are Apache-2.0; bundled fonts have their own licenses.

Zed is developed by Zed Industries and its contributors. EVB IDE modifications
are maintained by Eugene Barsky and contributors. Contributions to this fork
are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md).
