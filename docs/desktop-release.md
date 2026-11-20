# Desktop releases

Run **Actions → Desktop release → Run workflow** with `publish` off to get a Linux x86-64 installer artifact. Normal pushes do not release the app. Publish from `main` only; bump `Cargo.toml` and `Cargo.lock` together before the next release. The workflow builds on Ubuntu 24.04. The installer needs Bash, coreutils and `flock`; the application uses the desktop system libraries.

## Enable the release channel

The source repository can stay private. The built-in updater downloads without a GitHub login, so the destination must be a **public, binary-only** GitHub repository. This does make the downloads accessible to anyone with the link. Do not create or make a destination public unless that visibility is acceptable. With no public channel, leave the variables unset and distribute Actions artifacts manually to authorized users.

1. Initialize the destination repository (a README is enough). Then set source repository variables `SPOFIFY_UPDATE_REPO` to the exact `owner/repository` of the release destination and `SPOFIFY_UPDATE_PUBLIC_KEY` to the raw 32-byte Ed25519 public key in lowercase hex. To generate a key on a trusted machine:

   ```sh
   umask 077
   openssl genpkey -algorithm ED25519 -out spofify-release.pem
   openssl pkey -in spofify-release.pem -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n'
   ```

2. In the source repository, configure the `desktop-release` Actions environment for `main` and restrict approval to release maintainers. Set its secrets `SPOFIFY_RELEASE_SIGNING_KEY` (the PEM) and `SPOFIFY_RELEASE_PUBLISH_TOKEN` (a fine-grained token with Contents read/write on the destination repository only). Keep an offline backup of the signing key. Do not check it into Git or grant the token to installed clients.
3. Run the workflow from `main` with `publish` on. Only the tarball, executable and signed manifest are uploaded to the destination Releases. The first installer bootstraps the verification key: obtain it from the trusted release page, not an arbitrary mirror.

The app accepts only a newer stable version, binds its manifest to the configured repository and asset, and verifies the exact executable before atomically replacing it. The GUI and the old process are left alone. No GitHub credentials or Spofify sign-in details go to the updater. A signing-key change requires distributing a fresh installer; clients already installed trust the previous key. If a release must be withdrawn, publish a corrected build at a higher version. Re-running `install.sh` is also the manual recovery path. The auto-updater changes only the executable; rerun the installer if launcher or icon files need updating.
