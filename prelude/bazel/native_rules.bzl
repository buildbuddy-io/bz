load("@prelude//:alias.bzl", "alias_impl")
load("@prelude//:bazel_cc_toolchain.bzl", "cc_toolchain_impl", "cc_toolchain_suite_impl")
load("@prelude//:export_file.bzl", "export_file_impl")
load("@prelude//:sh_binary.bzl", "sh_binary_impl")
load("@prelude//:sh_test.bzl", "sh_test_impl")
load("@prelude//:test_suite.bzl", "test_suite_impl")
load("@prelude//apple:apple_platforms.bzl", "APPLE_PLATFORMS_KEY")
load("@prelude//configurations:rules.bzl", _config_extra_attributes = "extra_attributes", _config_implemented_rules = "implemented_rules")
load("@prelude//decls:common.bzl", "prelude_rule")
load("@prelude//decls:core_rules.bzl", "core_rules")
load("@prelude//decls:cxx_rules.bzl", "cxx_rules")
load("@prelude//decls:java_rules.bzl", "java_rules")
load("@prelude//decls:shell_rules.bzl", "shell_rules")
load("@prelude//genrule.bzl", "genrule_attributes")
load("@prelude//java:java.bzl", _java_implemented_rules = "implemented_rules")
load("@prelude//transitions:constraint_overrides.bzl", "constraint_overrides")

def _unimplemented(name, ctx):
    fail("Unimplemented rule type `{}` for target `{}`.".format(name, ctx.label))

def _unimplemented_impl(name):
    return partial(_unimplemented, name)

def _mk_rule(rule_spec: prelude_rule, extra_attrs: dict[str, Attr] = {}, impl_override = None, **kwargs):
    name = rule_spec.name
    attributes = dict(rule_spec.attrs)
    attributes.update(extra_attrs)
    attributes[APPLE_PLATFORMS_KEY] = attrs.dict(key = attrs.string(), value = attrs.dep(), sorted = False, default = {})

    impl = rule_spec.impl
    extra_impl = _implemented_rules.get(name)
    if extra_impl:
        if impl:
            fail("{} had an impl in the declaration and in the extra implemented rules".format(name))
        impl = extra_impl
    if not impl:
        impl = _unimplemented_impl(name)
    if impl_override != None:
        impl = impl_override

    extra_args = dict(kwargs)
    if rule_spec.cfg != None:
        extra_args["cfg"] = rule_spec.cfg
    if rule_spec.docs:
        extra_args["doc"] = rule_spec.docs
    if rule_spec.uses_plugins != None:
        extra_args["uses_plugins"] = rule_spec.uses_plugins
    if rule_spec.supports_incoming_transition != None:
        extra_args["supports_incoming_transition"] = rule_spec.supports_incoming_transition
    is_toolchain_rule = rule_spec.is_toolchain_rule
    if is_toolchain_rule == None:
        is_toolchain_rule = False
    extra_args.setdefault("is_configuration_rule", name in _config_implemented_rules)
    extra_args.setdefault("is_toolchain_rule", is_toolchain_rule)

    return rule(
        impl = impl,
        attrs = attributes,
        **extra_args
    )

_declared_rules = {
    "alias": core_rules.alias,
    "cc_binary": cxx_rules.cc_binary,
    "cc_import": cxx_rules.cc_import,
    "cc_library": cxx_rules.cc_library,
    "cc_shared_library": cxx_rules.cc_shared_library,
    "cc_test": cxx_rules.cc_test,
    "cc_toolchain": cxx_rules.cc_toolchain,
    "cc_toolchain_suite": cxx_rules.cc_toolchain_suite,
    "config_setting": core_rules.config_setting,
    "constraint_setting": core_rules.constraint_setting,
    "constraint_value": core_rules.constraint_value,
    "export_file": core_rules.export_file,
    "java_binary": java_rules.java_binary,
    "java_import": java_rules.java_import,
    "java_library": java_rules.java_library,
    "java_lite_proto_library": java_rules.java_lite_proto_library,
    "java_package_configuration": java_rules.java_package_configuration,
    "java_plugin": java_rules.java_plugin,
    "java_proto_library": java_rules.java_proto_library,
    "java_runtime": java_rules.java_runtime,
    "java_test": java_rules.java_test,
    "java_toolchain": java_rules.java_toolchain,
    "platform": core_rules.platform,
    "sh_binary": shell_rules.sh_binary,
    "sh_test": shell_rules.sh_test,
    "test_suite": core_rules.test_suite,
    "toolchain": core_rules.toolchain,
    "toolchain_type": core_rules.toolchain_type,
}

_extra_attributes = {
    "export_file": constraint_overrides.attributes,
    "genrule": genrule_attributes() | constraint_overrides.attributes,
    "sh_test": constraint_overrides.attributes,
} | _config_extra_attributes

_implemented_rules = {
    "alias": alias_impl,
    "cc_toolchain": cc_toolchain_impl,
    "cc_toolchain_suite": cc_toolchain_suite_impl,
    "export_file": export_file_impl,
    "sh_binary": sh_binary_impl,
    "sh_test": sh_test_impl,
    "test_suite": test_suite_impl,
} | _config_implemented_rules | _java_implemented_rules

rules = {
    name: _mk_rule(rule_spec, _extra_attributes.get(name, {}))
    for name, rule_spec in _declared_rules.items()
}

load_symbols(rules)
