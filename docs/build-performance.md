# Desktop build performance

## GitHub Actions baseline

Completed Ubuntu 24.04 runs before dependency caching, using GitHub job timestamps and Cargo's reported times. The first test-profile command includes crate downloads and compilation/linking; the test harness time is separate. The unoptimized build column is the subsequent normal debug executable build, not another full build of its dependencies.

| Run | Workflow wall time | Native setup | First test profile | Tests / MPRIS execution | Debug build / release build | Release package + upload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| [44810745554](https://github.com/Baizey/spofify-desktop/actions/runs/44810745554) Native Linux | 5m 39s | 12s | 4m 41s | 0.72s / 0.01s | 34.54s (debug) | — |
| [44815167484](https://github.com/Baizey/spofify-desktop/actions/runs/44815167484) Native Linux | 4m 21s | 13s | 3m 29s | 0.63s / 0.01s | 26.11s (debug) | — |
| [44807915605](https://github.com/Baizey/spofify-desktop/actions/runs/44807915605) Desktop release | 15m 57s | 13s | 5m 01s | 0.70s / 0.01s | 10m 22s (release) | 2s + 3s |
| [44808395316](https://github.com/Baizey/spofify-desktop/actions/runs/44808395316) Desktop release | 28m 28s | 12s | 4m 39s | 0.72s / 0.01s | 10m 27s (release) | 2s + 3s |

The second release waited 12m 54s for its package job to start while the first occupied the release concurrency group. Its package job itself took 15m 33s. The Cargo registry-update message preceded the first compile message by about 5s in the first release and 4s in the 5m 39s Native Linux run. This is a log-based estimate of startup/initial downloads, not an exclusive timing of all network activity.

In the first release, the log reached `Compiling spofify-desktop` about 47.9s before the test harness was built and 4m 34s before the production executable was built. Other compilation can overlap both intervals. Release compiles a second, optimized dependency graph, and the configured thin LTO still runs during production code generation. In contrast, the Native Linux debug executable shares dependencies with the test profile; only the application was listed as compiling on the subsequent debug build. MPRIS reused the test executable (Cargo reported 0.23–0.34s for the already-built profile). Removing it or other productive tests would not materially shorten CI.

## Local build and fixture measurements

Measured with Rust/Cargo 1.99.0, Ryzen 9 7950X (16 Cargo jobs), Linuxbrew ALSA/fontconfig, a populated Cargo registry, and fresh isolated `/dev/shm` target and fixture directories. Commands used `--locked --offline`; Cargo's `--timings` app-unit spans include both compilation and linking, not an isolated linker measurement.

| Local phase | Wall time |
| --- | ---: |
| `cargo fetch --target x86_64-unknown-linux-gnu` (offline lookup, no downloads) | 1.45s |
| Cold `cargo test --timings --no-run` (app unit: 18.96s) | 66.61s |
| Warm test-profile freshness check (Cargo reported, before tests) | 0.83s |
| Private-bus MPRIS via warm Cargo (test itself: 0.01s) | 1.12s |
| `cargo build` after tests (app unit: 17.41s); fully warm repeat | 18.38s; 0.91s |
| Cold `cargo build --release` (app unit: 39.92s); fully warm repeat | 95.79s; 0.95s |
| `cargo test --release` after the production build (wall time minus libtest execution, includes Cargo overhead) | 33.35s |

Median direct test-harness wall times over five runs, including process startup: offline 20ms (9 tests), storage 16ms (2), login store 15ms (7), collections 17ms (4), preferences 14ms (7), and engine 117ms (16). Locally, the sandbox denied TCP connections to ephemeral loopback API and updater fixtures. The ordinary full-suite attempt finished 167 passed, 28 failed, 1 ignored; the resulting ~30s timeout is not a passing-suite runtime or evidence that those fixtures are slow. The complete suite passed in the CI runs above. Formatting, the isolated MPRIS test, and Linux staging, desktop-file validation, and uninstall all passed locally.

## Cache scope and verification

Native Linux restores compiled Cargo dependencies after setting up stable. The pinned action leaves workspace outputs and Cargo binaries out of the cache; only a successful `main` push saves one. The debug app, Rust suite, private-bus MPRIS check, and install/uninstall validation still run. A new toolchain, changed build inputs, missing cache, or runner-image migration can make a run cold. Keep the Ubuntu label in the cache key in sync with `runs-on`.

For an actual CI comparison, record the cache seed's save time/size, then a warm run with the same runner, toolchain, and dependency inputs. Include restore time, Cargo compile time, post-job steps, and total wall time; a cache hit alone does not establish a speedup.

Other opportunities:

- Preserve the production profile and both test invocations. `cargo test --release` can share optimized dependencies with `cargo build --release`, but still compiles and links a separate harness and disables debug assertions by default. The local optimized harness build with warm release dependencies was faster than the cold debug test build, but that comparison does not establish the GitHub speedup or preserve debug-test coverage. The release test binary cannot substitute for the release executable.
- Time a dedicated cache for release dry runs separately; a Native Linux debug cache cannot supply optimized release dependencies, and a cache hit still leaves application codegen and LTO. Keep compiled caches out of the publishing path unless their trust and measured benefit justify using them.
- If cold release latency matters, independent test and production-build jobs can run in parallel, with publish gated on both. This trades extra setup and runner usage for lower wall time without changing profiles. Preserve installer smoke checks and release serialization; the 12m 54s wait was independent of compilation.
- A same-repository branch with an open PR can trigger both push and PR checks. Restricting push CI to `main` would avoid that overlap but also remove pre-PR branch checks. Keep both triggers unless that coverage trade-off is intentional.
