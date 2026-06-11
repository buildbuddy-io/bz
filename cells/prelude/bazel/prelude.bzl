load("@prelude//bazel:filegroup.bzl", "bazel_filegroup")
load("@prelude//bazel:genrule.bzl", "bazel_genrule")
load("@prelude//bazel:genquery.bzl", "bazel_genquery")
load("@prelude//bazel:native_rules.bzl", _rules = "rules")
load("@prelude//bazel:package_group.bzl", "bazel_package_group")
load("@prelude//bazel:starlark_doc_extract.bzl", "starlark_doc_extract")

def cc_libc_top_alias(name, visibility = None, **kwargs):
    attrs = dict(kwargs)
    attrs["name"] = name
    attrs["srcs"] = []
    if visibility != None:
        attrs["visibility"] = visibility
    bazel_filegroup(**attrs)

def _struct_to_dict(s):
    vals = {}
    for name in dir(s):
        vals[name] = getattr(s, name)
    return vals

_bazel_native_rule_backings = {
    "alias": _rules["alias"],
    "cc_binary": _rules["cc_binary"],
    "cc_import": _rules["cc_import"],
    "cc_library": _rules["cc_library"],
    "cc_libc_top_alias": cc_libc_top_alias,
    "cc_shared_library": _rules["cc_shared_library"],
    "cc_test": _rules["cc_test"],
    "cc_toolchain": _rules["cc_toolchain"],
    "cc_toolchain_suite": _rules["cc_toolchain_suite"],
    "config_setting": _rules["config_setting"],
    "constraint_setting": _rules["constraint_setting"],
    "constraint_value": _rules["constraint_value"],
    "export_file": _rules["export_file"],
    "filegroup": bazel_filegroup,
    "genquery": bazel_genquery,
    "genrule": bazel_genrule,
    "java_binary": _rules["java_binary"],
    "java_import": _rules["java_import"],
    "java_library": _rules["java_library"],
    "java_lite_proto_library": _rules["java_lite_proto_library"],
    "java_package_configuration": _rules["java_package_configuration"],
    "java_plugin": _rules["java_plugin"],
    "java_proto_library": _rules["java_proto_library"],
    "java_runtime": _rules["java_runtime"],
    "java_test": _rules["java_test"],
    "java_toolchain": _rules["java_toolchain"],
    "package_group": bazel_package_group,
    "platform": _rules["platform"],
    "sh_binary": _rules["sh_binary"],
    "sh_test": _rules["sh_test"],
    "starlark_doc_extract": starlark_doc_extract,
    "test_suite": _rules["test_suite"],
    "toolchain": _rules["toolchain"],
    "toolchain_type": _rules["toolchain_type"],
}
for _rule_name in ("cc_proto_library", "proto_library", "sh_library"):
    if _rule_name in _rules:
        _bazel_native_rule_backings[_rule_name] = _rules[_rule_name]

bz_bazel_native_rules = struct(**_bazel_native_rule_backings)

def exports_files(srcs, visibility = None, **_kwargs):
    visibility = visibility or ["//visibility:public"]
    for src in srcs:
        bz_bazel_native_rules.export_file(
            name = src,
            mode = "reference",
            src = src,
            visibility = visibility,
        )

_native = _struct_to_dict(__buck2_builtins__)
_native.update(_struct_to_dict(__buck2_builtins__.native))
_native["bz_bazel_native_rules"] = bz_bazel_native_rules
_native["exports_files"] = exports_files
native = struct(**_native)
