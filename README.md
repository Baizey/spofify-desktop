# Spofify Desktop

Linux-first native desktop client for Spofify, using Rust and Slint. Windows/macOS are possible future targets, not supported releases yet.

## Initial MVP brief

Build a useful native UI foundation: enter a server origin, sign in, browse albums and their tracks, and sign out. Prove screenshots, structured inspection and automated interaction against the actual Slint application. This first slice is read-only after authentication: playback, playlist editing, remembered credentials and release packaging are not implemented or implied.

Reuse the existing server API without changing the server or mobile application. Validate with disposable synthetic accounts/data, not live services. No remote repository, publication or deployment is requested.

Implementation and exact verification instructions will be added with the MVP.
