# Ubuntu diagnostic validation — 2026-09-30

The source-installed CLI passed the live **inspect → capture → report** smoke
test in a booted Ubuntu guest. This extends the earlier container result to a
real user systemd manager and cgroup v2 hierarchy.

## Environment and provenance

| Item | Tested value |
|---|---|
| Image | Ubuntu 24.04.5 LTS amd64 cloud image |
| Kernel | `6.8.0-142-generic` |
| systemd | `255.4-1ubuntu8.17` |
| Python | `3.12.3` |
| Rust / Cargo | `1.97.1` |
| VM | QEMU 11.0.0, KVM, 2 vCPUs, 4 GiB RAM, 12 GiB sparse disk |
| AMC | `0.1.0`, source-installed release build |

Image: <https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img>
with SHA-256
`6a81c37564db9b1ee84e141922625e1d7c5b389b99bb3c572e0243607d5bb4d2`,
verified against the same official directory's `SHA256SUMS`.

Source was `a7be0f3` plus the runner/report/Nix review fixes, isolated from
concurrent admission-service development. The exact saved `source.patch` has
SHA-256 `98b4a5ecf16d12f5d9e0867b62245bd9676bc5bc36a83aa2c24557a00292ba14`.
The installed executable's SHA-256 was
`42ef30603343bebab9ba38a061f766116c2bbbe2cae8ca580b2e472a19f63cf1`.
Subsequent sandbox-portability changes affect Rust tests only.

## Reproduction

In a disposable, booted guest with a working user manager, install
`build-essential`, `ca-certificates`, `curl`, Python 3.11+, and Rust. Then, as
the ordinary guest user:

```sh
cargo install --path . --locked -j 2
python3 scripts/test-validate-capture.py
python3 tests/linux-diagnostics.py \
  --amc "$HOME/.cargo/bin/amc" --output "$HOME/amc-diagnostic-proof"
```

The explicit smoke test creates two uniquely named sleep services, bounded by
120-second manager runtime limits. The normal service has a 64 MiB memory cap
and no swap. A private runtime slice disables its memory controller for the
unavailable-measurement case. Cleanup stops those fixtures and removes the
runtime slice file. This test is deliberately separate from ordinary offline
fixture checks.

## Results

| Case | Evidence |
|---|---|
| Install | `cargo install --path /src --locked -j 2` succeeded |
| Inspect | Manager settings and kernel max/swap limits agreed |
| Normal capture | 3/3 target readings, `reason=deadline`, `complete=true` |
| Interrupted capture | SIGTERM after readiness; one durable sample, `reason=cancelled`, `complete=false` |
| Unavailable controller | Three persisted samples, 0/3 known target readings, `reason=observer-not-ready`, `complete=false` |
| Reports | Installed CLI produced valid JSON and UTF-8 Markdown for all three cases |
| Passivity | The normal target remained active with the same invocation, cgroup, slice, and limits |

The unavailable case contained no invented target maximum; the report said
`memory.current: unavailable (0/3 samples)`. The interrupted case retained its
prefix and disclosed the unsupported single-point rates. Passive event
intervals reported `lifetimeComplete=false`.

Local evidence is retained under
`/data/scratch/tmp/opencode/amc-ubuntu-2404-booted/`: image/checksums,
`source.patch`, cloud-init seed, guest setup, console and installation logs,
and `exports/artifacts/diagnostics/` with the captures, inspections, reports,
and `result.json`. The guest powered off after exporting the result.

This validates the tested diagnostic workflow on this image. Upgrade/removal,
other Ubuntu versions, observer-overhead comparisons, managed admission, and
gaming outcomes require their own evidence.

## Review-fix verification on Atlas

The isolated `a7be0f3` plus review fixes passed 190 native workspace/all-target
tests, strict clippy, fixture regressions, and actual builds of all four
x86_64-linux Cargo check derivations. The first Nix test build exposed two
host-cgroup assumptions in tests; those now assert the appropriate unavailable
evidence outcome inside the sandbox as well as the live-host outcome.

Built outputs:

| Target | Store output |
|---|---|
| `fmt` | `/nix/store/kldpzaf43a79h28j16sdzdfps3bv7dlg-amc-fmt-0.1.0` |
| `clippy` | `/nix/store/p9182l3q4g5263vcdn3mcfiw6ns4xzya-amc-clippy-0.1.0` |
| `test` | `/nix/store/5kvgmskp8dl6p8shc5355r68x2v91xz4-amc-test-0.1.0` |
| `admission-features` | `/nix/store/d6haa9s53q6zsxyq7w9brz6l73dsc7d7-amc-admission-features-0.1.0` |
| default package | `/nix/store/20k0xkiwcfgklpijrym6plx1qiwjnqiq-amc-0.1.0` |

Commands used `canix cache binary build TARGET --no-push --max-jobs 1 --cores 2`
with `--include-tests` for check outputs. Packaged Markdown/JSON reports passed
against the recorded 1,800-sample Atlas capture and all three Ubuntu captures
with `PATH=/nonexistent`, verifying the packaged Python dependency. The broader
working tree, including concurrent admission-service work, separately passed
200 native Rust tests and strict clippy; those results do not extend this
snapshot's Nix or Ubuntu evidence to the new service.
