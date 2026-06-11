import argparse
import os
import shutil


RUN_LIVEPAR_MAIN_MODULE = "__run_lpar_main__"
SOURCE_SUFFIX = ".py"


def make_clean_dir(path):
    if os.path.isdir(path):
        shutil.rmtree(path)
    os.makedirs(path)


def get_target_info(target):
    return argparse.Namespace(
        raw=target,
        lib_path_env="LD_LIBRARY_PATH",
        lib_preload_env="LD_PRELOAD",
    )


def get_args_parser(*args, **kwargs):
    parser = argparse.ArgumentParser(*args, **kwargs)
    parser.add_argument("--argcomplete-hint", default="")
    parser.add_argument("--copy-files", action="store_true")
    parser.add_argument("--fix-os-argv", action="store_true")
    parser.add_argument("--interpreter-flags", action="append", default=[])
    parser.add_argument("--ld-preload", action="append", default=[])
    parser.add_argument("--main-runner", default="__run_lpar_main__.main")
    parser.add_argument("--output", required=True)
    parser.add_argument("--python", default="python3")
    parser.add_argument("--python-home")
    parser.add_argument("--runtime-binary")
    parser.add_argument("--runtime-env", action="append", default=[])
    parser.add_argument("--strict-tabs", action="store_true")
    parser.add_argument("--target", default="")
    parser.add_argument("--warnings")
    return parser


def get_user_main(options, manifest):
    main = getattr(options, "main", None) or getattr(options, "module", None)
    if not main:
        return ("__main__", "main")
    if ":" in main:
        return tuple(main.split(":", 1))
    if "." in main:
        return tuple(main.rsplit(".", 1))
    return (main, "main")


def make_ld_library_path(options, env_name, base_dirs):
    values = list(base_dirs)
    existing = os.environ.get(env_name)
    if existing:
        values.append(existing)
    return ":".join(values)


def make_ld_preload(env_name, entries, base_dir=None):
    resolved = []
    for entry in entries:
        if base_dir and not os.path.isabs(entry):
            resolved.append(os.path.join(base_dir, entry))
        else:
            resolved.append(entry)
    return " ".join(resolved)
