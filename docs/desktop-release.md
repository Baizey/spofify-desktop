# Desktop releases

Run **Actions → Desktop release → Run workflow** on `main`. Choose `build-only` to test the installer without publishing or configuring signing. Choose `initial` for the first release (`v0.1.0`). Published installers appear under **Releases** on this repository. Configure signing once before the first publish. Normal pushes run CI and do not publish releases.

For later releases, update the desktop package version in `Cargo.toml` and `Cargo.lock` together, merge to `main`, then choose:

| Choice | Use for | Example from `1.2.3` |
| --- | --- | --- |
| `major` | Breaking changes | `2.0.0` |
| `minor` | Backward-compatible features | `1.3.0` |
| `patch` | Backward-compatible fixes | `1.2.4` |

The workflow checks the exact bump against the highest stable published `vX.Y.Z` tag, ignores drafts and prereleases as baselines, and refuses existing tags. Use the workflow rather than creating releases or release tags manually. The installer targets Linux x86-64 and is built and smoke-tested on Ubuntu 24.04.

If a failed publish leaves a tag or draft, confirm that it belongs to the failed run and that the release was never made public. Then delete that draft (if present) and its exact tag before rerunning. Never reuse a publicly released version.

## Configure release signing once

The repository must be public so the updater can fetch release assets without an account. Generate an Ed25519 key on a trusted machine and keep the PEM outside the repository:

```sh
umask 077
openssl genpkey -algorithm ED25519 -out spofify-release.pem
openssl pkey -in spofify-release.pem -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n'
```

1. In **Settings → Secrets and variables → Actions → Variables**, add a repository variable named `SPOFIFY_UPDATE_PUBLIC_KEY` with the printed hex value.
2. In **Settings → Environments**, create `desktop-release`. Under **Deployment branches and tags**, select **Selected branches and tags** and add `main` as a branch. Add required reviewers if you want an additional approval before publishing.
3. In that environment’s **Environment secrets**, add `SPOFIFY_RELEASE_SIGNING_KEY` with the complete PEM contents, including the header and footer. Back up the key offline. Do not commit it or share it with users.
4. In **Settings → Rules → Rulesets**, create an active tag ruleset targeting `v*`. Enable **Restrict updates** and **Restrict deletions**, but leave creation unrestricted so the workflow can reserve tags. Limit any bypass to trusted maintainers recovering a failed, never-public release.

The workflow uses its own GitHub token to create the release tag and publish. No PAT or additional repository is needed.

The application embeds the public key. The workflow publishes the installer, executable, and signed manifest in the **same** GitHub Release. For a first install, download the archive from this repository’s GitHub Releases page. The signed manifest verifies subsequent in-app binary updates, not the installer archive itself.

## Installing updates

Spofify checks after sign-in and every four hours while online. A verified newer version shows **Update** at the top. The app downloads after you click and applies the update on your next launch. It never interrupts playback or restarts itself. You can also rerun `install.sh` from the latest release. Either way, settings and downloads remain in place.

The updater only offers strictly newer versions. If a release must be withdrawn, publish a corrected build with a higher version. If the signing key changes, existing users must reinstall from the trusted release page.
