# Desktop releases

## Publish

On [Baizey/spofify-desktop](https://github.com/Baizey/spofify-desktop), update the desktop package version in `Cargo.toml` and `Cargo.lock` together, then merge to `main`. Run **Actions → Desktop release → Run workflow** on `main` and choose the matching bump. Normal pushes run Native Linux CI and do not publish releases.

| Choice | Use for | From `1.2.3` | First release |
| --- | --- | --- | --- |
| `major` | Breaking changes | `2.0.0` | `1.0.0` |
| `minor` | Backward-compatible features | `1.3.0` | `0.1.0` |
| `patch` | Backward-compatible fixes | `1.2.4` | `0.0.1` |

The workflow checks the exact bump against the highest stable published `vX.Y.Z` tag, or `0.0.0` if there is none. Drafts and prereleases do not set the baseline. For a first release at `0.1.0`, select `minor`. Existing tags cannot be reused. Use the workflow rather than creating tags or releases manually.

The workflow builds and smoke-tests the Linux x86-64 installer on Ubuntu 24.04. It uploads exactly two assets to the same release: `spofify-desktop-VERSION-linux-x86_64.tar.gz` for installation and `spofify-desktop-linux-x86_64` for in-app updates.

## Protect the release channel

The canonical repository must be public so users can fetch updates without an account. The workflow checks that it is on `main` in that repository and uses the built-in GitHub Actions token to publish; no PAT, extra repository, or custom signing setup is needed.

1. In **Settings → Environments**, create `desktop-release`. Under **Deployment branches and tags**, select **Selected branches and tags** and allow only the `main` branch. Add required reviewers for an approval before publishing.
2. In **Settings → Rules → Rulesets**, create an active tag ruleset targeting `v*`. Enable **Restrict updates** and **Restrict deletions**, but leave creation unrestricted so the workflow can reserve tags. Limit any bypass to trusted maintainers recovering a failed, never-public release.

If publishing fails after a tag or draft is created, confirm that it belongs to the failed run and the release was never public. Delete that draft (if present) and its exact tag before rerunning. Never reuse a publicly released version. If a release needs to be replaced, publish a corrected build with a higher version; in-app updates do not roll back.

## Install and update

For a first install or a manual update, download the installer archive from the [official GitHub Releases page](https://github.com/Baizey/spofify-desktop/releases), extract it, and run `bash install.sh` in the extracted directory. Open **Spofify** from the app menu. No root access or Rust installation is needed.

The updater is enabled for optimized Linux x86-64 builds running from the release installer’s managed location, not debug builds or ordinary system installations. The installed app checks for a newer release after sign-in and periodically while online. Select **Update** to download it. When the update is ready, quit and reopen Spofify to run the new version; the app does not restart itself or interrupt playback. The app automatically makes a limited number of attempts if the download is interrupted. If the download still fails, no update appears, or your version predates GitHub digest updates, close Spofify and rerun the installer from the official Releases page. Your settings, sign-in, and saved music stay in place.

The updater trusts the publisher of `Baizey/spofify-desktop` and GitHub. It reads release metadata over HTTPS and checks the downloaded executable against the SHA-256 digest GitHub reports for that asset. This detects a mismatch with the published asset; it is not an independent signature and does not protect against a compromised publisher or GitHub. The app does not verify the installer archive.
