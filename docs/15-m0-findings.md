# 15 — M0 Findings

The written conclusion that M0's exit criterion asks for ([13](13-roadmap.md)). The pass
was time-boxed: primary documentation and issue trackers online, plus live evidence from
a real development machine (Arch Linux, .NET SDK 10.0.112, clang 22.1.8, three Steam
libraries on three volumes). Anything that was read rather than run says so.

Recorded 2026-09-11.

## Verdicts

| Spike | Verdict | Basis |
|---|---|---|
| Avalonia 12 + NativeAOT + CommunityToolkit.Mvvm | **Publish proven; launch pending** | Live publish of `MSBE.Desktop` |
| wasmtime capability sandbox | **Supported by primary docs; not executed** | wasmtime API documentation |
| Filesystem probes | **Proven on ext4 and NTFS; design change adopted** | Live probes |
| Proton / `localconfig.vdf` | **As bad as feared, and largely avoidable** | Live Steam install (read-only) and docs |
| CEF hosting | **Better than planned; design change adopted** | crates.io and CEF documentation |

## Decision: GO on the C# UI

Avalonia 12 with NativeAOT is the desktop UI.

- **Condition.** The AOT-published binary launches and renders its first window. This is
  the one remaining check on the decision (see [Still open](#still-open)).
- **Fallback, decided.** If AOT fails at runtime and trimmer roots cannot fix it, publish
  the same Avalonia app **self-contained without AOT**. That costs binary size and startup
  time, and nothing architectural.
- **Dropped.** A Rust-native UI is no longer carried as a fallback. The "AOT CLI" half of
  the old fallback was already satisfied: the CLI is a Rust binary.

## 1. Avalonia 12 + NativeAOT + CommunityToolkit.Mvvm

**Evidence (live).** `dotnet publish -c Release -r linux-x64` of the scaffolded
`MSBE.Desktop` (Avalonia 12.1.1, CommunityToolkit.Mvvm 8.4.2 partial properties, compiled
bindings, `TreatWarningsAsErrors`) **succeeded in 18 s with zero IL2xxx/IL3xxx trim or AOT
warnings**.

| Output | Size |
|---|---|
| `MSBE.Desktop` native executable | 20.0 MB |
| `libSkiaSharp.so` | 10.7 MB |
| `libHarfBuzzSharp.so` | 2.7 MB |
| **Shippable total** | **33.4 MB** |
| `MSBE.Desktop.dbg` (symbols, not shipped) | 51.6 MB |

Dynamic dependencies: `libc` and `libm` only.

**Evidence (docs).** Avalonia's Native AOT guide requires `PublishAot`, `IsAotCompatible`,
compiled bindings with `x:DataType`, and no `ReflectionBinding`, all of which this repo's
build already enforces. It presents `TrimmerRootAssembly` entries as a fix for reflection
errors at runtime, not a mandatory setting. CommunityToolkit's AOT problem with field-based
`[ObservableProperty]` (MVVMTK0045) is specific to WinRT (UWP and WinUI 3) and does not
affect Avalonia; this repo uses the partial-property form regardless.

**Not proven.** A clean publish does not prove the window renders, because trimming
failures surface at runtime. Startup time, the virtualized 1000-row list, and the Windows
and macOS publishes are unmeasured; CI's six-RID publish matrix covers the last.

## 2. wasmtime capability sandbox

**Evidence (docs); not executed.**

- **Fuel metering** is enabled on `Config` and consumed per `Store`. Components run in a
  `Store`, so metering applies to them with no extra work.
- **Resource limits.** A `ResourceLimiter`, attached with `Store::limiter`, caps memory,
  table and instance creation for everything in the store.
- **Capability denial is structural.** A component is instantiated against a `Linker`, and
  an import the host never defined fails instantiation ("unknown import … has not been
  defined"). An extension cannot reach a capability the host did not link, so there is no
  permission check to bypass.

That is the strongest form of the design in [02 §2.5](02-plan-system.md). It is cheaper to
prove inside M1, against a real fixture, than as a standalone spike.

## 3. Filesystem probes

**Evidence (live).** Real `FICLONE` (`cp --reflink=always`) and `link(2)` attempts using
temporary files, removed immediately:

| Volume | `stat -f` reported | Actual filesystem | Reflink | Hardlink |
|---|---|---|---|---|
| `/`, `/mnt/SSD` | `ext2/ext3` | ext4 | no: `Operation not supported` | yes |
| `/mnt/nvme` | **`fuse`** | **NTFS via ntfs-3g** (`fuseblk`) | no: `Operation not supported` | yes, same volume |
| home → `/mnt/nvme` | — | across volumes | — | no: `Invalid cross-device link` |
| `/tmp` | `tmpfs` | tmpfs | no | yes |

**Evidence (docs).** ext4 does not implement reflink; btrfs and XFS do. On Windows, block
cloning exists only on ReFS (including Dev Drive); NTFS has none, and `CreateHardLink` is
supported only on NTFS. SMB 3.0 supports hardlinks except on continuous-availability
shares, and NFS 4.2 cloning depends on the server's filesystem.

**Conclusions.**

1. **The probe tells the truth; the filesystem type does not.** `stat -f` reported an NTFS
   Steam library as `fuse`. Probing by attempting the operation, as
   [04 §4.2](04-deployment-engine.md) specifies, is necessary rather than merely cautious.
2. **Design change: the store is per volume.** This machine's three Steam libraries are on
   three volumes. A single store in `$XDG_DATA_HOME` could hardlink into one of them and
   would copy every file into the other two on every deploy. See
   [04 §4.1](04-deployment-engine.md).
3. **On Linux, hardlink is the realistic default, not reflink.** ext4 is the most common
   Linux filesystem, so hardlink's in-place-write hazard is the main case. Read-only store
   files, verification before linking, and copy-only mutable paths are load-bearing.

**Not proven live.** APFS `clonefile`, ReFS and Dev Drive, btrfs, XFS, and network shares.

## 4. Proton and `localconfig.vdf`

**Evidence (live, read-only).**

- **Steam was running** during the check, which is the normal state.
- One installed game already uses
  `"LaunchOptions" "WINEDLLOVERRIDES=\"version=n,b\" %command%"`: a DLL-proxy loader
  configured through launch options.
- No Proton prefix on the machine had a registry `DllOverrides` entry for a loader DLL.
- In the home library, 10 of 22 `compatdata` folders had no `pfx/user.reg`. A prefix's
  registry does not exist until the game has run under Proton.
- Registry files use `WINE REGISTRY Version 2` with timestamped section headers, and already
  contain per-executable keys such as `[Software\\Wine\\AppDefaults\\ActOfWar.exe] 1760832805`.

**Evidence (docs).**

- Steam reads `localconfig.vdf` only at startup and overwrites edits made while it runs.
  Valve's feature request for a programmatic mechanism,
  [steam-for-linux #6443](https://github.com/ValveSoftware/steam-for-linux/issues/6443),
  has been open since 11 August 2019 with no Valve response.
- BepInEx's **primary** documented Proton method is a `winhttp` DLL override set through
  `winecfg`, which is stored in the prefix registry and persists. The launch-option form is
  documented as the runtime-only alternative.
- Registry edits made while a prefix's Wine processes are running are lost: `wineserver`
  holds the registry in memory and writes it back.

**Conclusions.**

1. **The `localconfig.vdf` problem is confirmed**, and there is no Valve API to fall back on.
2. **It is avoidable for DLL-proxy loaders.** That covers BepInEx, ASI loaders, the
   `version.dll`/`winhttp.dll`/`dinput8.dll` proxy family, and the loader already on this
   machine. MSBE writes the override into the prefix registry, scoped to the game's
   executable under `AppDefaults`. The constraint shrinks from "Steam must be closed" to
   "this game must not be running," which can be checked per prefix.
3. **Two gaps remain.** A game never launched under Proton has no registry to write into,
   and loaders that need launch arguments rather than DLL overrides still require
   `localconfig.vdf` with Steam closed. See [08 §8.2](08-platforms-and-detection.md).

**Not proven live.** No loader was installed by hand, because that would have modified
installed games.

## 5. CEF hosting

**Evidence.** The `cef` crate from the Tauri team is at 152.1.0 (CEF and Chromium 152), was
last updated 2026-09-10, and has 221,576 downloads (108,250 recent). It supports Linux,
macOS and Windows on x86-64 and ARM64, and downloads CEF binaries at build time. A CEF
distribution is 100–200 MB, consistent with [07 §7.1](07-browser-and-secrets.md), and
multi-process isolation is inherent to CEF.

**Conclusion: design change.** `msbe-browser` is a **Rust** process built on `cef-rs`. That
keeps Chromium entirely out of the AOT-published C# process and removes the CefGlue plus
NativeAOT compatibility risk the original plan carried.

**Not proven.** No CEF build and no `nxm://` capture were attempted.

## Still open

The remaining M0 work, tracked in [13](13-roadmap.md):

1. **Launch the AOT-published desktop binary** and time startup. *Gates the GO.*
2. The virtualized 1000-row list under AOT.
3. Windows and macOS AOT publishes (covered by CI once it runs).
4. A wasmtime component that reads a fixture and emits operations, with one denied import.
5. Live probes on APFS, ReFS and Dev Drive, btrfs, XFS, and a network share.
6. One loader installed into a Proton prefix by hand, through the registry route.
7. A `cef-rs` build that captures an `nxm://` navigation.

## Sources

- [Avalonia — Native AOT](https://docs.avaloniaui.net/docs/deployment/native-aot)
- [.NET Community Toolkit 8.4 announcement](https://devblogs.microsoft.com/dotnet/announcing-the-dotnet-community-toolkit-840/) · [MVVMTK0045](https://github.com/MicrosoftDocs/CommunityToolkit/blob/main/docs/mvvm/generators/errors/MVVMTK0045.md)
- [wasmtime `Config`](https://docs.wasmtime.dev/api/wasmtime/struct.Config.html) · [component `Linker`](https://docs.wasmtime.dev/api/wasmtime/component/struct.Linker.html) · [`ResourceLimiter`](https://docs.wasmtime.dev/api/wasmtime/trait.ResourceLimiter.html)
- [FICLONE(2const)](https://www.man7.org/linux/man-pages/man2/FICLONE.2const.html) · [Windows Block Cloning](https://learn.microsoft.com/en-us/windows/win32/fileio/block-cloning) · [CreateHardLinkW](https://learn.microsoft.com/en-gb/windows/win32/api/winbase/nf-winbase-createhardlinkw)
- [Steam overwrites launch options edited while running](https://steamcommunity.com/discussions/forum/1/601913251731903059/) · [steam-for-linux #6443](https://github.com/ValveSoftware/steam-for-linux/issues/6443)
- [BepInEx — Running under Proton/Wine](https://docs.bepinex.dev/articles/advanced/proton_wine.html) · [BepInEx — Running games on Steam](https://docs.bepinex.dev/master/articles/advanced/steam_interop.html)
- [Wine registry changes lost while running](https://comp.emulators.ms-windows.wine.narkive.com/qQI39TyB/registry-changes-get-constantly-lost)
- [tauri-apps/cef-rs](https://github.com/tauri-apps/cef-rs) · [`cef` on crates.io](https://crates.io/crates/cef) · [CEF footprint discussion](https://magpcss.org/ceforum/viewtopic.php?f=6&t=15213)
