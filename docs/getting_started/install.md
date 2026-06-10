---
id: install
title: Installing Buck2
---

import { FbInternalOnly } from 'docusaurus-plugin-internaldocs-fb/internal';

<FbInternalOnly>

<h2>Internal Meta User</h2>

For Internal Meta Users, Buck2 is already configured and available for you.
Simply cloning the
[`fbsource`](https://www.internalfb.com/wiki/Repositories/fbsource/#cloning)
repository is all that's required to get started; no separate installation steps
for Buck2 are necessary.

If you have any issues, please check [here](../../users/faq/meta_installation).

</FbInternalOnly>

## Installing Buck2

The latest set of `buck2` executables can be found under the
[`latest` release page](https://github.com/facebook/buck2/releases/tag/latest).

Additionally, for each bi-monthly release there is a
[dotslash](https://dotslash-cli.com) file that is appropriate for committing to
a repository. This will automatically fetch the correct version and architecture
for each user, and ensures a consistent build environment for each commit in the
repo.

If no prebuilt binary is available for your platform — or you want to hack on
Buck2 itself — see [Building from Source](#building-from-source) below.

## Building from Source

Buck2 currently requires a nightly Rust toolchain. The simplest setup is via
[rustup](https://rustup.rs/), which provisions the right `rustc`/`cargo` for
you. Once it's installed, build and install `buck2` directly from GitHub:

```bash
rustup install nightly-2026-01-18
cargo +nightly-2026-01-18 install --git https://github.com/facebook/buck2.git buck2
```

This installs `buck2` into a suitable directory such as `$HOME/.cargo/bin`,
which you should add to your `$PATH`:

Linux / macOS

```sh
export PATH=$HOME/.cargo/bin:$PATH
```

Windows Powershell

```powershell
$Env:PATH += ";$HOME\.cargo\bin"
```

Verify the install with `buck2 --help`.

To hack on Buck2, build from a clone of the repo instead:

```sh
git clone https://github.com/facebook/buck2.git
cd buck2/
cargo install --path=app/bz
```

### `protoc` on non-Tier-1 platforms

Buck2 uses Protocol Buffers extensively, both internally and to talk to remote
systems for things like Remote Execution. Compiling the `.proto` files needs
the `protoc` compiler.

On Linux (aarch64/x86_64), Windows, and macOS the `cargo` build pulls a
prebuilt `protoc` from the
[`protoc-bin-vendored`](https://crates.io/crates/protoc-bin-vendored) crate, so
no setup is required.

On other operating systems, install `protoc` from another source (out of scope
here) and point the build at it before running `cargo build`:

- `BUCK2_BUILD_PROTOC` — path to the `protoc` binary
- `BUCK2_BUILD_PROTOC_INCLUDE` — path to the protocol buffers header directory

For example, with protobuf installed under `/opt/protobuf`:

```bash
export BUCK2_BUILD_PROTOC=/opt/protobuf/bin/protoc
export BUCK2_BUILD_PROTOC_INCLUDE=/opt/protobuf/include
```

### Building Buck2 with Buck2

See [Bootstrapping](../about/bootstrapping.md) for details. The gist:

```sh
buck2 build //:bz
```
