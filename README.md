# snowcone
Very similar to mixing all the 7-11 slushee flavors together, but with linux package managers.

## Usage

```sh
snow                        # open TUI
snow install NAME[@VERSION] # install packages
snow remove NAME            # remove packages
snow upgrade [NAME…]        # upgrade packages, or everything when none given
snow search QUERY           # search available packages across managers
snow info NAME              # show package metadata
snow list [--outdated]      # list installed (or outdated) packages
snow refresh                # refresh package indexes
snow managers               # show detected managers and capabilities
```

Global flags: `-m <your manager, ie: pacman>` to target specific backends, `--json` for machine output, `-y` to assume yes, `--dry-run` to preview.



## Distro Native Managers

| Package manager | Distro(s) | Package format | Notes |
|---|---|---|---|
| `apt` / `apt-get` / `aptitude` | Debian, Ubuntu, Linux Mint, Pop!_OS, elementary OS, Kali, Raspberry Pi OS, MX Linux | `.deb` | High-level frontends over dpkg |
| `dpkg` | Debian family | `.deb` | Low-level installer; no dependency resolution |
| `nala` | Debian, Ubuntu | `.deb` | Modern third-party frontend for apt |
| `apt-rpm` | PCLinuxOS, ALT Linux | `.rpm` | Port of apt for RPM systems |
| `rpm` | Red Hat family | `.rpm` | Low-level installer; no dependency resolution |
| `dnf` / `dnf5` | Fedora, RHEL 8+, Rocky, AlmaLinux, Amazon Linux 2023, OpenMandriva | `.rpm` | Successor to yum |
| `yum` | RHEL/CentOS 7 and earlier, Amazon Linux 2 | `.rpm` | Predecessor of dnf; legacy |
| `zypper` | openSUSE, SUSE Linux Enterprise | `.rpm` | Paired with the YaST management suite |
| `urpmi` | Mageia (formerly Mandriva) | `.rpm` | |
| `pacman` | Arch Linux, EndeavourOS, Manjaro, Garuda, Artix, SteamOS 3 | `.pkg.tar.zst` | |
| AUR helpers: `yay`, `paru`, `pikaur`, `trizen`, `aura` | Arch family | AUR build scripts | Build packages from the Arch User Repository |
| `pamac` | Manjaro | `.pkg.tar.zst` + more | CLI/GUI over pacman, AUR, Flatpak, Snap |
| `apk` (apk-tools) | Alpine, postmarketOS, Adélie, Chimera Linux | `.apk` | Chimera uses the newer apk3 |
| `xbps` (`xbps-install`) | Void Linux | `.xbps` | |
| `emerge` (Portage) | Gentoo, Funtoo, Calculate Linux | ebuilds (source) | Source-based with USE flags; binary packages also supported |
| `cave` (Paludis) | Exherbo, Gentoo | exheres / ebuilds | Alternative package mangler |
| `nix` | NixOS | Nix store paths | Declarative, functional, atomic rollbacks |
| `guix` | GNU Guix System | Guix store paths | Functional, Scheme-based; GNU counterpart to Nix |
| `eopkg` | Solus | `.eopkg` | Fork of PiSi |
| `pisi` | Pardus (historical), PisiLinux | `.pisi` | |
| `swupd` | Clear Linux | bundles | Bundle-based rather than per-package |
| `rpm-ostree` | Fedora Silverblue/Kinoite/CoreOS | OSTree commits + `.rpm` layering | Image-based, atomic updates |
| `transactional-update` | openSUSE MicroOS/Aeon | `.rpm` via snapshots | Atomic updates using btrfs snapshots + zypper |
| `pkgtools` (`installpkg`, `upgradepkg`, `removepkg`) | Slackware | `.txz` / `.tgz` | Low-level; no dependency resolution |
| `slackpkg` | Slackware | `.txz` / `.tgz` | Official update frontend |
| `slapt-get`, `slpkg`, `sbopkg` | Slackware | `.txz` / SlackBuilds | Third-party tools adding deps / SlackBuilds.org support |
| `netpkg` | Zenwalk | `.txz` | Slackware-based |
| `opkg` | OpenWrt, embedded Linux (Yocto) | `.ipk` | Lightweight, fork of ipkg |
| `tce-load` | Tiny Core Linux | `.tcz` | Extensions loaded into RAM |
| Puppy Package Manager (`petget`) | Puppy Linux | `.pet`, `.sfs` | |
| `prt-get` / `pkgutils` | CRUX | ports (source) | BSD-style ports system |
| `kiss` | KISS Linux | source | Minimalist shell-based package system |
| `scratchpkg` | Venom Linux | source | |
| `sorcery` (`cast` / `dispel`) | Source Mage GNU/Linux | "spells" (source) | |
| `lunar` (`lin` / `lrm`) | Lunar Linux | "modules" (source) | Shares ancestry with Sorcery (both from Sorcerer) |
| `Compile` / `InstallPackage` | GoboLinux | recipes / binaries | Unique `/Programs` filesystem layout |
| `luet` | MocaccinoOS | container-built packages | Successor community to Sabayon |
| `apx` | Vanilla OS | wraps other formats | Installs via distrobox containers (apt, dnf, apk inside) |
| `pacstall` | Ubuntu | pacscripts | "AUR for Ubuntu" |
| `makedeb` / MPR | Debian, Ubuntu | `.deb` | makepkg-style builds; Makedeb Package Repository |
| `eepm` (`epm`) | ALT Linux, cross-distro | wraps native formats | Unified CLI over many native package managers |

## Distro Agnostic Managers

| Package manager | Scope | Notes |
|---|---|---|
| Flatpak | Desktop apps | Sandboxed; primary hub is Flathub |
| Snap (`snapd`) | Desktop + server apps | Canonical-run store |
| AppImage | Portable apps | Single-file bundles; managed via `appimaged`, AppImageLauncher, `zap`, `AM` |
| Homebrew (Linuxbrew) | CLI tools, apps | macOS package manager ported to Linux |
| Nix (standalone) | Everything | Runs on any distro, not just NixOS |
| Guix (standalone) | Everything | Runs on any distro, not just Guix System |
| pkgsrc | Everything | NetBSD's portable packaging system |
| 0install (Zero Install) | Apps | Decentralized distribution |
| conda / mamba / micromamba / pixi | Data science, general binaries | Language-agnostic environments |
| Spack | HPC / scientific software | Combinatorial versioning for clusters |
| EasyBuild | HPC / scientific software | |
| GNU Stow | Local installs | Symlink-farm manager for `/usr/local` or dotfiles |
| PackageKit (`pkcon`) | Abstraction layer | Common API over native backends; used by GNOME Software / KDE Discover |
| bauh | GUI manager | Manages Flatpak, Snap, AppImage, AUR, web apps |

## Historical / discontinued

| Package manager | Origin | Notes |
|---|---|---|
| `up2date` | Red Hat Linux / early RHEL | Replaced by yum |
| `rug` / Red Carpet | Ximian → Novell/SUSE | ZENworks-era updater |
| Smart Package Manager | Cross-distro | Unified apt/yum/urpmi frontend |
| Conary | Foresight Linux, rPath | Version-control-inspired packaging |
| CNR ("Click'N'Run") | Linspire/Freespire | Early app-store model |
| Autopackage | Cross-distro | Distro-neutral installers |
| Listaller | Cross-distro | Merged concepts into AppStream/Limba, then abandoned |
| klik | Cross-distro | Predecessor of AppImage |
| `ipkg` | Handhelds/embedded (iPAQ, NSLU2) | Predecessor of opkg |
| Entropy (`equo`) | Sabayon Linux | Binary manager atop Gentoo; distro discontinued |
| `pacman-g2` | Frugalware | Fork of early pacman; distro discontinued |
| `swaret` | Slackware | Third-party updater, long unmaintained |

## Language / ecosystem package managers

These run on Linux regardless of distro and manage packages for a language runtime.

| Ecosystem | Package manager(s) |
|---|---|
| Python | `pip`, `pipx`, `uv`, Poetry, PDM, Hatch, conda |
| JavaScript / Node | `npm`, Yarn, `pnpm`, Bun, Deno (JSR) |
| Rust | Cargo (`cargo`, `cargo-binstall`) |
| Go | `go install` / Go modules |
| Ruby | RubyGems (`gem`), Bundler |
| PHP | Composer, PEAR/PECL |
| Perl | CPAN (`cpan`, `cpanm`, `cpm`) |
| Java / JVM | Maven, Gradle, Ant+Ivy, Coursier |
| Scala | sbt |
| Clojure | Leiningen, tools.deps |
| .NET | NuGet (`dotnet`), Paket |
| Haskell | Cabal, Stack |
| OCaml | opam |
| Elixir / Erlang | Mix + Hex, rebar3 |
| Lua | LuaRocks |
| Dart / Flutter | pub |
| D | dub |
| Nim | Nimble |
| Zig | `build.zig.zon` / `zig fetch` |
| Crystal | Shards |
| Swift | Swift Package Manager |
| Fortran | fpm |
| Julia | Pkg |
| R | `install.packages`, pak, renv, BiocManager |
| TeX | `tlmgr` (TeX Live), `mpm` (MiKTeX) |
| C / C++ | vcpkg, Conan, xmake/xrepo |
| Haxe | haxelib |
| Racket | `raco pkg` |
| Common Lisp | Quicklisp |
| V | vpm |
| Kubernetes | Helm (charts) |

## Related tooling (honorable mentions)

Not package managers in the strict sense, but adjacent:

- **GUI storefronts / frontends:** GNOME Software, KDE Discover, Synaptic, Octopi, dnfdragora, Muon
- **Editor package managers:** `package.el` (Emacs, ELPA/MELPA), lazy.nvim / vim-plug (Vim/Neovim)
- **Shell plugin managers:** zinit, antigen (zsh), Fisher (fish), bpkg / basher (bash)
- **Toolchain / version managers:** asdf, mise, rustup, SDKMAN!, nvm, pyenv, rbenv
