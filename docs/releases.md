# Release packaging

Run `cargo xtask dist` from a checkout to build a native Linux release and the
Files WASI component, then create a tarball and a SHA-256 manifest in
`target/dist`. Run `cargo xtask dist --deb` to also produce a Debian package.
Set `PROTEUS_DEB_MAINTAINER="Name <email@example.org>"` before building a public
Debian release; the default is placeholder contributor metadata.
The supported host architectures are x86_64 and aarch64. Cross compilation is
not supported by this task. Builds use the checked-in Cargo lockfiles.

The `Release distributions` GitHub workflow runs when a version tag such as
`v0.1.0` is pushed. The tag must exactly match `v` followed by the version in
`workspace.package.version` in `Cargo.toml`. It builds natively on Ubuntu 24.04
for x86_64 and aarch64 using Rust 1.98.1, creates tarballs, Debian packages, and
SHA-256 manifests with the distribution task, verifies the checksums, and uploads
the files as separate architecture artifacts on the Actions run. Download the
corresponding `proteus-dist-TAG-ARCH` artifact to obtain the distribution files.
Set the repository Actions variable `PROTEUS_DEB_MAINTAINER` to the maintainer's
`Name <email@example.org>` before building public Debian releases; otherwise the
packaging task's placeholder contributor metadata is used.

Requirements: the workspace Rust toolchain, the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`), native development libraries for Wayland,
xkbcommon and fontconfig, and `tar`. Debian packaging also requires `dpkg-deb`.
Use `--skip-build` to package existing artifacts from
`target/<host-triple>/release/proteus` and
`target/wasm32-wasip2/release/proteus_files.wasm`.
This option does not check artifact freshness.

The tarball contains a versioned directory with a `usr` tree. Copy that tree
into an installation prefix (for example, copy its contents into `/usr/local`),
or install the Debian package using `sudo apt install ./proteus_VERSION_ARCH.deb`.
A Wayland session, working GPU drivers, fontconfig, xkbcommon, and the Wayland
client libraries are required at runtime. These are dynamically linked native
builds; build on the oldest distribution you intend to support. Debian dependency
names describe the required libraries, but do not promise compatibility with
older libc versions. Test on each supported distribution before publishing.

Desktop integration uses `cc.spie.proteus`, matching the Wayland app ID.
The supplied SVG is the desktop icon; original logos and wordmarks are included
under `share/proteus/branding`. Documentation, an example workspace, the
licence (`LICENSE`) and the licences of every bundled third-party component
(`THIRD-PARTY-NOTICES.txt`) are in `share/doc/proteus`; the Debian package also
has the `copyright` file there. Packaging fails if a bundled crate's licence
text cannot be found. Shell hooks are embedded in the executable; terminfo is
installed automatically in the user's data directory at runtime (requires `tic`).
The terminfo source is also supplied under `share/proteus`.

The Files plugin is shipped in `share/proteus/plugins/files`. Enable it explicitly
in your Lua configuration with the installed directory (adjust the prefix for
local installations):

```lua
require("proteus").load_plugin { source = "/usr/share/proteus/plugins/files" }
```

It is not enabled automatically. The prebuilt component requires no Rust toolchain
on the installed machine. Optional preview and file-operation helpers follow the
plugin manifest permissions; install the helpers you need separately.

Check downloads with `sha256sum -c proteus-VERSION-HOST.SHA256SUMS` from the
artifact directory. Checksums detect corruption; releases are not signed by
this task. Preserve older packages to roll back by reinstalling them. Back up
personal configuration before changing versions; no migration or rollback of
user data is performed. Tar installations can be removed by deleting the files
listed by `tar -tzf`; Debian installations can be removed with `apt remove proteus`.
User configuration and data are retained.
