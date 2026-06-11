# Minimal local replacement for the old grpc_library macro.

def grpc_library(name, srcs, languages = [], visibility = ["PUBLIC"], **kwargs):
    native.filegroup(
        name = name,
        srcs = srcs,
        visibility = visibility,
        **kwargs
    )

    if "py" in languages:
        native.filegroup(
            name = name + "-py",
            srcs = srcs,
            visibility = visibility,
        )
