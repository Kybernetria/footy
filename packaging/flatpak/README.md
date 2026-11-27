# Footy Flatpak

Footy is distributed as a Flatpak from a signed repository on GitHub Pages, for
x86_64. The commands below work once the repository owner has
[enabled publication](#owner-setup).

## Install and update

Open <https://kybernetria.github.io/footy/> and choose **Install Footy**, or run:

```bash
flatpak install --user --from https://kybernetria.github.io/footy/footy.flatpakref
flatpak run computer.footy.app
flatpak update --user computer.footy.app
```

KDE Discover and GNOME Software list updates from the same repository. The GNOME
runtime comes from Flathub. To roll back, pick one of the two older commits
shown by `flatpak remote-info --user --log footy computer.footy.app` and run
`flatpak update --user --commit=COMMIT computer.footy.app`. Use the remote name
shown by `flatpak info --user --show-origin computer.footy.app` if it is not
`footy`.

## Sandbox

| Permission | Used for |
| --- | --- |
| `--socket=pulseaudio` | Microphone capture and feedback sounds |
| `--socket=wayland`, `--socket=fallback-x11`, `--share=ipc` | Windows and the recording overlay |
| `--device=dri` | GPU-accelerated rendering |
| `--share=network` | Speech model downloads and optional post-processing |
| `--talk-name=org.kde.StatusNotifierWatcher` | Tray icon |

Footy has no access to your home directory, input devices or other applications'
D-Bus services. Settings, models, history and recordings stay in
`~/.var/app/computer.footy.app/`, separate from a native installation. Import
audio files with the file picker, which grants access to the chosen file.

The tray icon needs a StatusNotifier host. KDE Plasma has one; GNOME needs an
AppIndicator extension.

Microphone selection follows your sound server. If nothing is recorded, check
the input device in the desktop's sound settings.

## Desktop integration

On Wayland, Footy uses XDG desktop portals, and the desktop decides what is
allowed. The settings are under **Desktop integration** in the app.

- **Global shortcuts** are requested from the desktop, which also delivers key
  release for push-to-talk. Choose the keys in the desktop's shortcut settings.
- **Paste automatically** is off until you turn it on. The desktop then asks for
  keyboard and clipboard access. Without it, Footy copies the transcript while its
  window is focused, and otherwise saves it to History and shows a notification.
- **Launch on startup** needs the Background portal.
- Updates come from Flatpak; the built-in updater is off.

## Shortcut commands

Global shortcuts depend on what the desktop's portal supports. Any desktop can
instead bind these commands in its own shortcut settings. They talk to the
running app; the first two start recording, and stop it when run again.

```bash
flatpak run computer.footy.app --toggle-transcription
flatpak run computer.footy.app --toggle-post-process
flatpak run computer.footy.app --cancel
```

## Build and check locally

Needs `flatpak`, `flatpak-builder`, `ostree`, `gpg`, `appstreamcli`,
`desktop-file-validate` and the Flathub remote. Run from the repository root,
with a work directory outside it:

```bash
export FLATPAK_BUILDER_ARGS='--install-deps-from=flathub --user'
bash packaging/flatpak/scripts/check-source.sh
bash packaging/flatpak/scripts/build.sh ~/footy-flatpak
bash packaging/flatpak/scripts/check-installed.sh ~/footy-flatpak
bash packaging/flatpak/scripts/test-signed-repo.sh ~/footy-flatpak
flatpak install --user --bundle ~/footy-flatpak/footy.flatpak
```

If you use the Flathub build of the builder, set
`FLATPAK_BUILDER="flatpak run org.flatpak.Builder"`.

The two check scripts install into temporary directories and need the GNOME 51
runtime in the system installation. If yours is in the user installation, prefix
them with `FLATPAK_SYSTEM_DIR="$HOME/.local/share/flatpak"`.

The build stage has network access so that Bun and Cargo can fetch the versions
in `bun.lock` and `Cargo.lock`. Bun itself, the native libraries and the vendored
[shared modules](shared-modules/README.md) are pinned in the manifest. The
`ort-sys` crate downloads ONNX Runtime and verifies its SHA-256.

When the application version changes, add a matching `<release>` to
`computer.footy.app.metainfo.xml`; the source check requires it.

## How publication works

The [Flatpak workflow](../../.github/workflows/flatpak.yml) has three jobs:

1. **Build** runs on pull requests and `main`. It has no secrets. It builds,
   installs and checks the app, tests publication with a temporary key, and
   uploads an unsigned repository holding the one application commit.
2. **Sign** runs only on `main` when publication is enabled. It imports that
   commit as untrusted data, verifies its checksum, ref and permissions, checks
   that it follows the published commit, signs the commit and repository, and
   keeps two older commits.
3. **Deploy** publishes the signed site. Only this job can write to Pages.

A change to the sandbox permissions must update both the manifest and the
allowlist in [`scripts/check.py`](scripts/check.py).

## Owner setup

These steps need repository administrator access.

1. In **Settings → Pages**, set **Source** to **GitHub Actions**.
2. In **Settings → Environments**, create `flatpak-signing`. Under **Deployment
   branches and tags**, allow only `main`.
3. On a trusted computer, create a signing key used for nothing else. The
   workflow needs a key without a passphrase.

   ```bash
   export GNUPGHOME="$(mktemp -d)"
   gpg --batch --passphrase '' \
     --quick-generate-key 'Footy Flatpak repository' rsa4096 sign never
   gpg --list-secret-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }'
   ```

4. Store the key and its fingerprint as secrets of that environment. Replace
   `FINGERPRINT` with the output of the last command above.

   ```bash
   gpg --armor --export-secret-keys FINGERPRINT |
     gh secret set FLATPAK_GPG_PRIVATE_KEY --env flatpak-signing --repo Kybernetria/footy
   gh secret set FLATPAK_GPG_KEY_ID --env flatpak-signing --repo Kybernetria/footy \
     --body FINGERPRINT
   ```

5. Back up the key somewhere safe and offline, then delete the temporary
   directory. Installed copies trust this key only.
6. In **Settings → Secrets and variables → Actions → Variables**, add
   `PUBLISH_FLATPAK_REPO` with the value `true`. Set `FLATPAK_REPO_URL` only if
   the site is not served from `https://kybernetria.github.io/footy`, for example
   with a custom domain. It must be an HTTPS URL.
7. Run **Actions → Flatpak → Run workflow** on `main`, then install from the
   published page.

Protect `main`: anyone who can change it can publish a signed build.

### Publication problems

- *The build does not continue the published commit*: the published site changed,
  or was still cached, while the workflow ran. Wait a few minutes and run the
  whole workflow again.
- *The build is older than the published commit*: an old run was repeated. Start
  a new run on `main`.
- Replacing a lost or compromised key: update both secrets, then run the workflow
  with **reset_history** selected. Existing installations must remove the old
  remote and install again.
