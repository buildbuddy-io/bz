---
id: bootstrapping
title: Bootstrapping Buck2
---

# Bootstrapping Buck2

Buck2 can be built with `cargo` or `buck2`. The source repository includes
[DotSlash](https://dotslash-cli.com) files for `buck2` itself, so that you can
quickly self-bootstrap the build. This is particularly useful if you're writing
patches and need to test both builds.

For dependencies on Rust crates from [crates.io](https://crates.io), we use
[reindeer](https://github.com/facebookincubator/reindeer) to automatically
generate `BUCK` files.

Note that the resulting binary will be compiled without optimisations or
[jemalloc](https://github.com/jemalloc/jemalloc), so we recommend using the
Cargo-produced binary in further development.

First, install `dotslash` with `Cargo`:

```sh
cargo install --locked dotslash
```

Build a copy of `buck2` with `buck2`:

```sh
buck2 build //:bz
```
