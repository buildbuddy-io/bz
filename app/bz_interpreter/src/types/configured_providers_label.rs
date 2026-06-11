/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::hash::Hash;

use allocative::Allocative;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::paths::CellRelativePath;
use bz_core::package::PackageLabel;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::provider::label::NonDefaultProvidersName;
use bz_core::provider::label::ProvidersLabel;
use bz_core::provider::label::ProvidersName;
use bz_core::target::label::label::TargetLabel;
use bz_core::target::name::TargetNameRef;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use dupe::Dupe;
use pagable::Pagable;
use serde::Serialize;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::StarlarkPagable;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueError;
use starlark::values::none::NoneOr;
use starlark::values::starlark_value;

use crate::types::bazel::label_display::starlark_configured_providers_label_str;
use crate::types::bazel::label_display::starlark_providers_label_str;
use crate::types::cell_path::StarlarkCellPath;
use crate::types::cell_root::CellRoot;
use crate::types::package_path::StarlarkPackagePath;
use crate::types::project_root::StarlarkProjectRoot;
use crate::types::target_label::StarlarkConfiguredTargetLabel;
use crate::types::target_label::StarlarkTargetLabel;

fn bazel_repo_name_for_cell(cell: &str) -> String {
    if cell == "root" {
        return String::new();
    }
    bzlmod_canonical_repo_name_for_cell(cell).unwrap_or_else(|| cell.to_owned())
}

fn bazel_workspace_root_for_cell(cell: &str) -> String {
    if cell == "root" {
        String::new()
    } else {
        format!("external/{}", bazel_repo_name_for_cell(cell))
    }
}

fn bazel_label_relative_target(
    base_package: PackageLabel,
    label: &str,
) -> bz_error::Result<TargetLabel> {
    if label.starts_with('@') {
        return Err(bz_error::bz_error!(
            bz_error::ErrorTag::Input,
            "Label.relative does not support repository-qualified labels yet: `{}`",
            label
        ));
    }

    let (package, target_name) = if let Some(label) = label.strip_prefix("//") {
        let (package, target_name) = label
            .split_once(':')
            .map(|(package, target_name)| (package, target_name.to_owned()))
            .unwrap_or_else(|| {
                let target_name = label.rsplit('/').next().unwrap_or(label).to_owned();
                (label, target_name)
            });
        let package_path = CellRelativePath::new(ForwardRelativePath::new(package)?);
        (
            PackageLabel::new(base_package.cell_name(), package_path)?,
            target_name,
        )
    } else if let Some(target_name) = label.strip_prefix(':') {
        (base_package, target_name.to_owned())
    } else {
        (base_package, label.to_owned())
    };

    let target_name = TargetNameRef::new(&target_name)?;
    Ok(TargetLabel::new(package, target_name))
}

impl StarlarkConfiguredProvidersLabel {
    pub fn label(&self) -> &ConfiguredProvidersLabel {
        &self.label
    }
}

/// Container for `ConfiguredProvidersLabel` that gives users access to things like package, cell, etc. This can also be properly stringified by our forthcoming `CommandLine` object
#[derive(
    Clone,
    Debug,
    Trace,
    Freeze,
    ProvidesStaticType,
    Allocative,
    StarlarkPagable
)]
#[repr(C)]
pub struct StarlarkConfiguredProvidersLabel {
    #[freeze(identity)]
    #[starlark_pagable(pagable)]
    label: ConfiguredProvidersLabel,
}

impl fmt::Display for StarlarkConfiguredProvidersLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&starlark_configured_providers_label_str(&self.label))
    }
}

starlark_simple_value!(StarlarkConfiguredProvidersLabel);

impl Serialize for StarlarkConfiguredProvidersLabel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&starlark_configured_providers_label_str(&self.label))
    }
}

impl StarlarkConfiguredProvidersLabel {
    pub fn new(label: ConfiguredProvidersLabel) -> Self {
        StarlarkConfiguredProvidersLabel { label }
    }

    pub fn inner(&self) -> &ConfiguredProvidersLabel {
        &self.label
    }

    pub fn starlark_label_string(&self) -> String {
        starlark_configured_providers_label_str(&self.label)
    }
}

#[starlark_value(type = "Label", skip_pagable)]
impl<'v> StarlarkValue<'v> for StarlarkConfiguredProvidersLabel
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(configured_label_methods)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(match StarlarkConfiguredProvidersLabel::from_value(other) {
            Some(other) => self.label == other.label,
            None => false,
        })
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.label.hash(hasher);
        Ok(())
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
        if let Some(other) = StarlarkConfiguredProvidersLabel::from_value(other) {
            Ok(self.label.cmp(&other.label))
        } else {
            ValueError::unsupported_with(self, "compare", other)
        }
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&starlark_configured_providers_label_str(&self.label));
    }
}

/// A label is used to represent a configured target.
#[starlark_module]
fn configured_label_methods(builder: &mut MethodsBuilder) {
    /// For the label `//hello:world (ovr_config//platform/linux:x86_64-workspace-46b26edb4b80a905)` this gives back `buck2/hello`
    #[starlark(attribute)]
    fn package<'v>(
        this: &'v StarlarkConfiguredProvidersLabel,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(heap.alloc_str_intern(this.label.target().pkg().cell_relative_path().as_str()))
    }

    /// For the label `//hello:world (ovr_config//platform/linux:x86_64-workspace-46b26edb4b80a905)` this gives back `world`
    #[starlark(attribute)]
    fn name<'v>(this: &'v StarlarkConfiguredProvidersLabel) -> starlark::Result<&'v str> {
        Ok(this.label.target().name().as_str())
    }

    #[starlark(attribute)]
    fn sub_target<'v>(
        this: &'v StarlarkConfiguredProvidersLabel,
    ) -> starlark::Result<NoneOr<Vec<&'v str>>> {
        Ok(match this.label.name() {
            ProvidersName::Default => NoneOr::None,
            ProvidersName::NonDefault(flavor) => match flavor.as_ref() {
                NonDefaultProvidersName::Named(names) => {
                    NoneOr::Other(names.iter().map(|p| p.as_str()).collect())
                }
                NonDefaultProvidersName::UnrecognizedFlavor(_) => {
                    unreachable!(
                        "This should have been an error when looking up the corresponding analysis (`{}`)",
                        this.label
                    )
                }
            },
        })
    }

    /// For the label `//hello:world (ovr_config//platform/linux:x86_64-workspace-46b26edb4b80a905)` this gives back `//hello`
    #[starlark(attribute)]
    fn path<'v>(this: &StarlarkConfiguredProvidersLabel) -> starlark::Result<StarlarkCellPath> {
        Ok(StarlarkCellPath(this.label.target().pkg().to_cell_path()))
    }

    /// For the label `//hello:world (ovr_config//platform/linux:x86_64-workspace-46b26edb4b80a905)` this gives back `workspace`
    #[starlark(attribute)]
    fn cell<'v>(this: &'v StarlarkConfiguredProvidersLabel) -> starlark::Result<&'v str> {
        Ok(this.label.target().pkg().cell_name().as_str())
    }

    #[starlark(attribute)]
    fn repo_name(this: &StarlarkConfiguredProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_repo_name_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    #[starlark(attribute)]
    fn workspace_name(this: &StarlarkConfiguredProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_repo_name_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    #[starlark(attribute)]
    fn workspace_root(this: &StarlarkConfiguredProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_workspace_root_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    /// Returns the PackagePath for this configured providers label.
    #[starlark(attribute)]
    fn package_path<'v>(
        this: &StarlarkConfiguredProvidersLabel,
    ) -> starlark::Result<StarlarkPackagePath> {
        Ok(StarlarkPackagePath::new(this.label.target().pkg().dupe()))
    }

    /// Obtain a reference to this target label's cell root. This can be used as if it were an
    /// artifact in places that expect one, such as `cmd_args().relative_to`.
    #[starlark(attribute)]
    fn cell_root<'v>(this: &StarlarkConfiguredProvidersLabel) -> starlark::Result<CellRoot> {
        Ok(CellRoot::new(this.label.target().pkg().cell_name()))
    }

    /// Obtain a reference to the project's root. This can be used as if it were an artifact in
    /// places that expect one, such as `cmd_args().relative_to`.
    #[starlark(attribute)]
    fn project_root<'v>(
        this: &StarlarkConfiguredProvidersLabel,
    ) -> starlark::Result<StarlarkProjectRoot> {
        Ok(StarlarkProjectRoot)
    }

    /// For the label `//hello:world (ovr_config//platform/linux:x86_64-workspace-46b26edb4b80a905)` this returns the unconfigured underlying target label (`//hello:world`)
    fn raw_target(
        this: &StarlarkConfiguredProvidersLabel,
    ) -> starlark::Result<StarlarkTargetLabel> {
        Ok(StarlarkTargetLabel::new(
            (*this.label.target().unconfigured()).dupe(),
        ))
    }

    /// Returns the underlying configured target label, dropping the sub target
    fn configured_target(
        this: &StarlarkConfiguredProvidersLabel,
    ) -> starlark::Result<StarlarkConfiguredTargetLabel> {
        Ok(StarlarkConfiguredTargetLabel::new(
            (*this.label.target()).dupe(),
        ))
    }

    /// Creates a label in the same package as this label with the given target name.
    fn same_package_label(
        this: &StarlarkConfiguredProvidersLabel,
        target_name: &str,
    ) -> starlark::Result<StarlarkConfiguredProvidersLabel> {
        let target_name = TargetNameRef::new(target_name)?;
        let target = TargetLabel::new(this.label.target().pkg(), target_name)
            .configure_pair(this.label.target().cfg_pair().dupe());
        Ok(StarlarkConfiguredProvidersLabel::new(
            ConfiguredProvidersLabel::default_for(target),
        ))
    }

    /// Resolve a label string relative to this label's repository/package, following Bazel's Label.relative API.
    fn relative(
        this: &StarlarkConfiguredProvidersLabel,
        label: &str,
    ) -> starlark::Result<StarlarkConfiguredProvidersLabel> {
        let target = bazel_label_relative_target(this.label.target().pkg(), label)?
            .configure_pair(this.label.target().cfg_pair().dupe());
        Ok(StarlarkConfiguredProvidersLabel::new(
            ConfiguredProvidersLabel::default_for(target),
        ))
    }
}

impl StarlarkProvidersLabel {
    pub fn label(&self) -> &ProvidersLabel {
        &self.label
    }

    pub fn starlark_label_string(&self) -> String {
        starlark_providers_label_str(&self.label)
    }
}

/// Container for `ProvidersLabel` that gives users access to things like package, cell, etc.
#[derive(
    Clone,
    Debug,
    Trace,
    Freeze,
    ProvidesStaticType,
    Allocative,
    Serialize,
    Pagable,
    StarlarkPagable
)]
#[repr(C)]
#[serde(transparent)]
pub struct StarlarkProvidersLabel {
    #[freeze(identity)]
    #[starlark_pagable(pagable)]
    label: ProvidersLabel,
}

impl fmt::Display for StarlarkProvidersLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&starlark_providers_label_str(&self.label))
    }
}

starlark_simple_value!(StarlarkProvidersLabel);

impl StarlarkProvidersLabel {
    pub fn new(label: ProvidersLabel) -> Self {
        StarlarkProvidersLabel { label }
    }
}

#[starlark_value(type = "Label", skip_pagable)]
impl<'v> StarlarkValue<'v> for StarlarkProvidersLabel
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(label_methods)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        if let Some(other) = StarlarkProvidersLabel::from_value(other) {
            Ok(self.label == other.label)
        } else {
            Ok(false)
        }
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.label.hash(hasher);
        Ok(())
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
        if let Some(other) = StarlarkProvidersLabel::from_value(other) {
            Ok(self.label.cmp(&other.label))
        } else {
            ValueError::unsupported_with(self, "compare", other)
        }
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&starlark_providers_label_str(&self.label));
    }
}

#[starlark_module]
fn label_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn name<'v>(this: &'v StarlarkProvidersLabel) -> starlark::Result<&'v str> {
        Ok(this.label.target().name().as_str())
    }

    #[starlark(attribute)]
    fn sub_target<'v>(this: &'v StarlarkProvidersLabel) -> starlark::Result<NoneOr<Vec<&'v str>>> {
        Ok(match this.label.name() {
            ProvidersName::Default => NoneOr::None,
            ProvidersName::NonDefault(flavor) => match flavor.as_ref() {
                NonDefaultProvidersName::Named(names) => {
                    NoneOr::Other(names.iter().map(|p| p.as_str()).collect())
                }
                NonDefaultProvidersName::UnrecognizedFlavor(_) => {
                    unreachable!(
                        "This should have been an error when looking up the corresponding analysis (`{}`)",
                        this.label
                    )
                }
            },
        })
    }

    #[starlark(attribute)]
    fn path<'v>(this: &StarlarkProvidersLabel) -> starlark::Result<StarlarkCellPath> {
        Ok(StarlarkCellPath(this.label.target().pkg().to_cell_path()))
    }

    #[starlark(attribute)]
    fn cell<'v>(this: &'v StarlarkProvidersLabel) -> starlark::Result<&'v str> {
        let cell = this.label.target().pkg().cell_name().as_str();
        Ok(cell)
    }

    #[starlark(attribute)]
    fn repo_name(this: &StarlarkProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_repo_name_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    #[starlark(attribute)]
    fn workspace_name(this: &StarlarkProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_repo_name_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    #[starlark(attribute)]
    fn workspace_root(this: &StarlarkProvidersLabel) -> starlark::Result<String> {
        Ok(bazel_workspace_root_for_cell(
            this.label.target().pkg().cell_name().as_str(),
        ))
    }

    #[starlark(attribute)]
    fn package<'v>(
        this: &'v StarlarkProvidersLabel,
        heap: Heap<'v>,
    ) -> starlark::Result<StringValue<'v>> {
        Ok(heap.alloc_str_intern(this.label.target().pkg().cell_relative_path().as_str()))
    }

    /// Returns the PackagePath for this providers label.
    #[starlark(attribute)]
    fn package_path<'v>(this: &StarlarkProvidersLabel) -> starlark::Result<StarlarkPackagePath> {
        Ok(StarlarkPackagePath::new(this.label.target().pkg().dupe()))
    }

    /// Returns the unconfigured underlying target label.
    fn raw_target(this: &StarlarkProvidersLabel) -> starlark::Result<StarlarkTargetLabel> {
        Ok(StarlarkTargetLabel::new((*this.label.target()).dupe()))
    }

    /// Creates a label in the same package as this label with the given target name.
    fn same_package_label(
        this: &StarlarkProvidersLabel,
        target_name: &str,
    ) -> starlark::Result<StarlarkProvidersLabel> {
        let target_name = TargetNameRef::new(target_name)?;
        let target = TargetLabel::new(this.label.target().pkg(), target_name);
        Ok(StarlarkProvidersLabel::new(ProvidersLabel::default_for(
            target,
        )))
    }

    /// Resolve a label string relative to this label's repository/package, following Bazel's Label.relative API.
    fn relative(
        this: &StarlarkProvidersLabel,
        label: &str,
    ) -> starlark::Result<StarlarkProvidersLabel> {
        let target = bazel_label_relative_target(this.label.target().pkg(), label)?;
        Ok(StarlarkProvidersLabel::new(ProvidersLabel::default_for(
            target,
        )))
    }
}

// TODO(nga): remove the `Label` alias. (T264813434)
#[starlark_module]
#[starlark_types(
    StarlarkProvidersLabel as ProvidersLabel,
    StarlarkConfiguredProvidersLabel as ConfiguredProvidersLabel
)]
pub fn register_providers_label(globals: &mut GlobalsBuilder) {}

#[cfg(test)]
mod tests {
    use bz_core::cells::external::bzlmod_cell_name;
    use bz_core::cells::external::register_bzlmod_cell_canonical_repo_name;
    use bz_core::configuration::data::ConfigurationData;
    use bz_core::provider::label::ConfiguredProvidersLabel;
    use bz_core::provider::label::NonDefaultProvidersName;
    use bz_core::provider::label::ProviderName;
    use bz_core::provider::label::ProvidersLabel;
    use bz_core::provider::label::ProvidersName;
    use bz_core::target::configured_target_label::ConfiguredTargetLabel;
    use bz_core::target::label::label::TargetLabel;
    use bz_util::arc_str::ArcSlice;
    use starlark::assert::Assert;
    use starlark::environment::GlobalsBuilder;
    use starlark::starlark_module;

    use crate::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
    use crate::types::configured_providers_label::StarlarkProvidersLabel;

    #[starlark_module]
    fn register_test_providers_label(globals: &mut GlobalsBuilder) {
        fn configured_providers_label() -> starlark::Result<StarlarkConfiguredProvidersLabel> {
            Ok(StarlarkConfiguredProvidersLabel {
                label: ConfiguredProvidersLabel::new(
                    ConfiguredTargetLabel::testing_parse(
                        "foo//bar:baz",
                        ConfigurationData::testing_new(),
                    ),
                    ProvidersName::NonDefault(triomphe::Arc::new(NonDefaultProvidersName::Named(
                        ArcSlice::new([
                            ProviderName::new("qux".to_owned())?,
                            ProviderName::new("quux".to_owned())?,
                        ]),
                    ))),
                ),
            })
        }

        fn providers_label() -> starlark::Result<StarlarkProvidersLabel> {
            Ok(StarlarkProvidersLabel {
                label: ProvidersLabel::new(
                    TargetLabel::testing_parse("foo//bar:baz"),
                    ProvidersName::NonDefault(triomphe::Arc::new(NonDefaultProvidersName::Named(
                        ArcSlice::new([
                            ProviderName::new("qux".to_owned())?,
                            ProviderName::new("quux".to_owned())?,
                        ]),
                    ))),
                ),
            })
        }

        fn bzlmod_providers_label() -> starlark::Result<StarlarkProvidersLabel> {
            let cell_name = bzlmod_cell_name("rules_python+");
            Ok(StarlarkProvidersLabel {
                label: ProvidersLabel::default_for(TargetLabel::testing_parse(&format!(
                    "{cell_name}//python/config_settings:add_srcs_to_runfiles"
                ))),
            })
        }

        fn bzlmod_configured_providers_label() -> starlark::Result<StarlarkConfiguredProvidersLabel>
        {
            let cell_name = bzlmod_cell_name("rules_python+");
            Ok(StarlarkConfiguredProvidersLabel {
                label: ConfiguredProvidersLabel::default_for(ConfiguredTargetLabel::testing_parse(
                    &format!("{cell_name}//python/config_settings:add_srcs_to_runfiles"),
                    ConfigurationData::testing_new(),
                )),
            })
        }

        fn command_line_option_label() -> starlark::Result<StarlarkProvidersLabel> {
            Ok(StarlarkProvidersLabel {
                label: ProvidersLabel::default_for(TargetLabel::testing_parse(
                    "root//command_line_option:platforms",
                )),
            })
        }

        fn unregistered_bzlmod_providers_label() -> starlark::Result<StarlarkProvidersLabel> {
            Ok(StarlarkProvidersLabel {
                label: ProvidersLabel::default_for(TargetLabel::testing_parse(
                    "bzlmod_unknown_root_upb//:defs.bzl",
                )),
            })
        }

        fn root_label() -> starlark::Result<StarlarkProvidersLabel> {
            Ok(StarlarkProvidersLabel {
                label: ProvidersLabel::default_for(TargetLabel::testing_parse("root//:go.mod")),
            })
        }
    }

    #[test]
    fn test_configured_providers_label_to_json() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'\"foo//bar:baz[qux][quux]\"'",
            "json.encode(configured_providers_label())",
        );
    }

    #[test]
    fn test_providers_label_to_json() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'\"foo//bar:baz[qux][quux]\"'",
            "json.encode(providers_label())",
        );
    }

    #[test]
    fn test_providers_label_starlark_type_is_bazel_label() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq("'Label'", "type(providers_label())");
    }

    #[test]
    fn test_label_workspace_root() {
        register_bzlmod_cell_canonical_repo_name("rules_python+");

        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq("''", "root_label().workspace_root");
        a.eq("''", "command_line_option_label().workspace_root");
        a.eq(
            "'external/rules_python+'",
            "bzlmod_providers_label().workspace_root",
        );
    }

    #[test]
    fn test_root_label_repo_name() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq("''", "root_label().repo_name");
        a.eq("''", "root_label().workspace_name");
    }

    #[test]
    fn test_bzlmod_providers_label_str() {
        register_bzlmod_cell_canonical_repo_name("rules_python+");

        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'@@rules_python+//python/config_settings:add_srcs_to_runfiles'",
            "str(bzlmod_providers_label())",
        );
    }

    #[test]
    fn test_unregistered_bzlmod_providers_label_str() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'@@unknown_root_upb//:defs.bzl'",
            "str(unregistered_bzlmod_providers_label())",
        );
    }

    #[test]
    fn test_bzlmod_configured_providers_label_str() {
        register_bzlmod_cell_canonical_repo_name("rules_python+");

        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'@@rules_python+//python/config_settings:add_srcs_to_runfiles'",
            "str(bzlmod_configured_providers_label())",
        );
        a.eq(
            "'\"@@rules_python+//python/config_settings:add_srcs_to_runfiles\"'",
            "json.encode(bzlmod_configured_providers_label())",
        );
    }

    #[test]
    fn test_command_line_option_label_str() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'//command_line_option:platforms'",
            "str(command_line_option_label())",
        );
    }

    #[test]
    fn test_configured_providers_label_same_package_label() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'foo//bar:package.json'",
            "str(configured_providers_label().same_package_label('package.json'))",
        );
    }

    #[test]
    fn test_providers_label_same_package_label() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'foo//bar:package.json'",
            "str(providers_label().same_package_label('package.json'))",
        );
    }

    #[test]
    fn test_configured_providers_label_relative() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'foo//bar:patches/fix.patch'",
            "str(configured_providers_label().relative('patches/fix.patch'))",
        );
        a.eq(
            "'foo//other/pkg:file.txt'",
            "str(configured_providers_label().relative('//other/pkg:file.txt'))",
        );
    }

    #[test]
    fn test_providers_label_relative() {
        let mut a = Assert::new();
        a.globals_add(register_test_providers_label);
        a.eq(
            "'foo//bar:patches/fix.patch'",
            "str(providers_label().relative('patches/fix.patch'))",
        );
        a.eq(
            "'foo//other/pkg:file.txt'",
            "str(providers_label().relative('//other/pkg:file.txt'))",
        );
    }
}
