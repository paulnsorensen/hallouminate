# hallouminate

[![GitHub](https://img.shields.io/badge/GitHub-paulnsorensen/hallouminate-blue?logo=github)](https://github.com/paulnsorensen/hallouminate)

Thin npm shim around the Rust binary. `postinstall` downloads the
prebuilt platform binary from the matching GitHub release.

```sh
npx hallouminate --help
```

Supported platforms: macOS (x64 / arm64), Linux (x64 / arm64, glibc).
For Windows, musl Linux, or other targets, install from source with
`cargo install hallouminate`.

See the [repository](https://github.com/paulnsorensen/hallouminate) for
documentation, configuration, and MCP integration.

License: MIT.
