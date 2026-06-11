# Third-Party Buck Cell

This cell is reserved for local Buck definitions for external dependencies.

The hard cutover away from Meta-internal cells rewrites old `root//third-party/...`
labels to this cell. The actual Rust, Python, protobuf, and tool targets still
need to be wired to the repository's chosen vendoring/package-management flow.
