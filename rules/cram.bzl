# Minimal local replacement for the old cram_test wrapper.

def cram_test(name, srcs, env = {}, **kwargs):
    native.sh_test(
        name = name,
        srcs = srcs,
        env = env,
        **kwargs
    )
