# Releasing XR MultiCast

Releases are built by `.github/workflows/release.yml` from the scripts in `packaging/` (see
[packaging/README.md](../packaging/README.md)). It builds Windows, macOS, Linux (x86_64 and
aarch64) and Android packages, writes `SHA256SUMS.txt` and creates a **draft** GitHub release.

## Cutting a release

1. Set the version in the root `Cargo.toml` (`[workspace.package] version`), run `cargo check`
   so `Cargo.lock` follows, and add a `## [X.Y.Z]` section to `CHANGELOG.md` (it becomes the
   release notes). Commit and push to `main`.
2. Optional dry run: Actions → Release → Run workflow (keep "Only build the packages" ticked),
   or `gh workflow run release.yml --ref main`. The packages appear as the run's
   `xr-multicast-release` artifact.
3. Tag and push:

   ```bash
   git tag -a vX.Y.Z -m "XR MultiCast X.Y.Z"
   git push origin vX.Y.Z
   ```

   The tag must match the `Cargo.toml` version, or the release job fails.
4. When the run is green, open the draft under Releases, check the notes and files, and
   publish it.

## Android signing secrets

Without these secrets the APKs are signed with the runner's throwaway debug key, the release
notes say so, and users can't update in place between releases. Create the key once and keep
the keystore and passwords safe (a lost key means users must uninstall to update):

```bash
keytool -genkeypair -v -keystore xr-multicast-release.jks -alias xr-multicast \
  -keyalg RSA -keysize 4096 -validity 10000 \
  -dname "CN=XR MultiCast, O=Dunk Technologies"
```

Then add four repository secrets (Settings → Secrets and variables → Actions, or with gh):

```bash
base64 -w0 xr-multicast-release.jks | gh secret set ANDROID_KEYSTORE_BASE64   # macOS: base64 -i xr-multicast-release.jks
gh secret set ANDROID_KEYSTORE_PASSWORD     # prompts for the value
gh secret set ANDROID_KEY_ALIAS --body xr-multicast
gh secret set ANDROID_KEY_PASSWORD          # same as the store password for PKCS12 keystores
```

Local release builds use the same key through environment variables:

```bash
XRMC_KEYSTORE=xr-multicast-release.jks XRMC_KEYSTORE_PASSWORD=… XRMC_KEY_ALIAS=xr-multicast \
  apps/xrmc-android/build.sh release --abi all
```

## Windows and macOS signing

The Windows and macOS packages aren't code-signed. Windows SmartScreen shows "Windows protected
your PC" (More info → Run anyway), and macOS Gatekeeper blocks the first start (right-click →
Open, or `xattr -dr com.apple.quarantine "/Applications/XR MultiCast.app"`). `build-macos.sh`
signs with `MACOS_SIGN_IDENTITY` when set; notarization and Authenticode signing aren't set up.
