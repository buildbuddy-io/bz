/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory.
 * You may select, at your option, one of the above-listed licenses.
 */

use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::ValueTyped;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcInternal;

impl fmt::Display for BazelCcInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cc_internal")
    }
}

starlark::starlark_simple_value!(BazelCcInternal);

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcToolchainFeatures {
    selectables: Vec<BazelSelectable>,
    default_selectables: Vec<String>,
    action_tools: Vec<BazelActionTool>,
    artifact_name_patterns: Vec<BazelArtifactNamePattern>,
    tools_directory: String,
}

impl fmt::Display for BazelCcToolchainFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<CcToolchainFeatures>")
    }
}

starlark::starlark_simple_value!(BazelCcToolchainFeatures);

#[starlark_value(type = "CcToolchainFeatures")]
impl<'v> StarlarkValue<'v> for BazelCcToolchainFeatures {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_toolchain_features_methods)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelFeatureConfiguration {
    requested_features: Vec<String>,
    enabled_selectables: Vec<String>,
    action_tools: Vec<BazelActionTool>,
    tools_directory: String,
}

impl fmt::Display for BazelFeatureConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<FeatureConfiguration({})>",
            self.requested_features.join(", ")
        )
    }
}

starlark::starlark_simple_value!(BazelFeatureConfiguration);

#[derive(Debug, Clone, Allocative)]
struct BazelSelectable {
    name: String,
    requires: Vec<Vec<String>>,
    implies: Vec<String>,
    provides: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelActionTool {
    action_name: String,
    path: String,
    path_origin: BazelToolPathOrigin,
    with_features: Vec<BazelWithFeatureSet>,
    execution_requirements: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
enum BazelToolPathOrigin {
    CrosstoolPackage,
    FilesystemRoot,
    WorkspaceRoot,
}

#[derive(Debug, Clone, Allocative)]
struct BazelWithFeatureSet {
    features: Vec<String>,
    not_features: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelArtifactNamePattern {
    category: String,
    prefix: String,
    extension: String,
}

#[derive(Debug)]
struct BazelArtifactCategory {
    name: &'static str,
    default_prefix: &'static str,
    default_extension: &'static str,
    allowed_extensions: &'static [&'static str],
}

const BAZEL_CC_ARTIFACT_CATEGORIES: &[BazelArtifactCategory] = &[
    BazelArtifactCategory {
        name: "static_library",
        default_prefix: "lib",
        default_extension: ".a",
        allowed_extensions: &[".a", ".lib"],
    },
    BazelArtifactCategory {
        name: "alwayslink_static_library",
        default_prefix: "lib",
        default_extension: ".lo",
        allowed_extensions: &[".lo", ".lo.lib"],
    },
    BazelArtifactCategory {
        name: "dynamic_library",
        default_prefix: "lib",
        default_extension: ".so",
        allowed_extensions: &[".so", ".dylib", ".dll", ".pyd", ".wasm"],
    },
    BazelArtifactCategory {
        name: "executable",
        default_prefix: "",
        default_extension: "",
        allowed_extensions: &["", ".exe", ".wasm"],
    },
    BazelArtifactCategory {
        name: "interface_library",
        default_prefix: "lib",
        default_extension: ".ifso",
        allowed_extensions: &[".ifso", ".tbd", ".if.lib", ".lib"],
    },
    BazelArtifactCategory {
        name: "pic_file",
        default_prefix: "",
        default_extension: ".pic",
        allowed_extensions: &[".pic"],
    },
    BazelArtifactCategory {
        name: "included_file_list",
        default_prefix: "",
        default_extension: ".d",
        allowed_extensions: &[".d"],
    },
    BazelArtifactCategory {
        name: "serialized_diagnostics_file",
        default_prefix: "",
        default_extension: ".dia",
        allowed_extensions: &[".dia"],
    },
    BazelArtifactCategory {
        name: "object_file",
        default_prefix: "",
        default_extension: ".o",
        allowed_extensions: &[".o", ".obj"],
    },
    BazelArtifactCategory {
        name: "pic_object_file",
        default_prefix: "",
        default_extension: ".pic.o",
        allowed_extensions: &[".pic.o"],
    },
    BazelArtifactCategory {
        name: "cpp_module",
        default_prefix: "",
        default_extension: ".pcm",
        allowed_extensions: &[".pcm", ".gcm", ".ifc"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_info",
        default_prefix: "",
        default_extension: ".CXXModules.json",
        allowed_extensions: &[".CXXModules.json"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_ddi",
        default_prefix: "",
        default_extension: ".ddi",
        allowed_extensions: &[".ddi"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_modmap",
        default_prefix: "",
        default_extension: ".modmap",
        allowed_extensions: &[".modmap"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_modmap_input",
        default_prefix: "",
        default_extension: ".modmap.input",
        allowed_extensions: &[".modmap.input"],
    },
    BazelArtifactCategory {
        name: "generated_assembly",
        default_prefix: "",
        default_extension: ".s",
        allowed_extensions: &[".s", ".asm"],
    },
    BazelArtifactCategory {
        name: "processed_header",
        default_prefix: "",
        default_extension: ".processed",
        allowed_extensions: &[".processed"],
    },
    BazelArtifactCategory {
        name: "generated_header",
        default_prefix: "",
        default_extension: ".h",
        allowed_extensions: &[".h"],
    },
    BazelArtifactCategory {
        name: "preprocessed_c_source",
        default_prefix: "",
        default_extension: ".i",
        allowed_extensions: &[".i"],
    },
    BazelArtifactCategory {
        name: "preprocessed_cpp_source",
        default_prefix: "",
        default_extension: ".ii",
        allowed_extensions: &[".ii"],
    },
    BazelArtifactCategory {
        name: "coverage_data_file",
        default_prefix: "",
        default_extension: ".gcno",
        allowed_extensions: &[".gcno"],
    },
    BazelArtifactCategory {
        name: "clif_output_proto",
        default_prefix: "",
        default_extension: ".opb",
        allowed_extensions: &[".opb"],
    },
];

fn bazel_cc_error(message: impl Into<String>) -> starlark::Error {
    starlark::Error::new_other(std::io::Error::other(message.into()))
}

fn sequence_values<'v>(value: Value<'v>, field: &str) -> starlark::Result<Vec<Value<'v>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    Err(bazel_cc_error(format!(
        "Expected `{field}` to be a list or tuple, got `{}`",
        value.get_type()
    )))
}

fn string_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Option<String>> {
    let Some(attr) = value.get_attr(name, heap)? else {
        return Ok(None);
    };
    if attr.is_none() {
        return Ok(None);
    }
    attr.unpack_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected `{name}` to be a string, got `{}`",
                attr.get_type()
            ))
        })
}

fn bool_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<bool> {
    let Some(attr) = value.get_attr(name, heap)? else {
        return Ok(false);
    };
    if attr.is_none() {
        return Ok(false);
    }
    attr.unpack_bool().ok_or_else(|| {
        bazel_cc_error(format!(
            "Expected `{name}` to be a bool, got `{}`",
            attr.get_type()
        ))
    })
}

fn string_sequence(value: Value<'_>, field: &str) -> starlark::Result<Vec<String>> {
    sequence_values(value, field)?
        .into_iter()
        .map(|value| {
            value
                .unpack_str()
                .map(|value| value.to_owned())
                .ok_or_else(|| {
                    bazel_cc_error(format!(
                        "Expected `{field}` entries to be strings, got `{}`",
                        value.get_type()
                    ))
                })
        })
        .collect()
}

fn string_sequence_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Vec<String>> {
    let Some(attr) = value.get_attr(name, heap)? else {
        return Ok(Vec::new());
    };
    string_sequence(attr, name)
}

fn push_unique(values: &mut Vec<String>, value: String) -> bool {
    if values.iter().any(|existing| existing == &value) {
        false
    } else {
        values.push(value);
        true
    }
}

fn parse_with_feature_set<'v>(
    value: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<BazelWithFeatureSet> {
    Ok(BazelWithFeatureSet {
        features: string_sequence_attr(value, "features", heap)?,
        not_features: string_sequence_attr(value, "not_features", heap)?,
    })
}

fn parse_requires<'v>(
    value: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Vec<Vec<String>>> {
    let Some(requires) = value.get_attr("requires", heap)? else {
        return Ok(Vec::new());
    };
    sequence_values(requires, "requires")?
        .into_iter()
        .map(|feature_set| string_sequence_attr(feature_set, "features", heap))
        .collect()
}

fn parse_tool<'v>(
    action_name: &str,
    tool: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<BazelActionTool> {
    let (path, path_origin) = if let Some(path) = string_attr(tool, "path", heap)? {
        let path_origin = if path.starts_with('/') {
            BazelToolPathOrigin::FilesystemRoot
        } else {
            BazelToolPathOrigin::CrosstoolPackage
        };
        (path, path_origin)
    } else if let Some(tool_artifact) = tool.get_attr("tool", heap)? {
        let path = string_attr(tool_artifact, "path", heap)?.ok_or_else(|| {
            bazel_cc_error("Expected action_config tool artifact to expose a `path` attribute")
        })?;
        (path, BazelToolPathOrigin::WorkspaceRoot)
    } else {
        return Err(bazel_cc_error(
            "Expected action_config tool to provide exactly one of `path` or `tool`",
        ));
    };

    let with_features = if let Some(value) = tool.get_attr("with_features", heap)? {
        sequence_values(value, "with_features")?
            .into_iter()
            .map(|value| parse_with_feature_set(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    Ok(BazelActionTool {
        action_name: action_name.to_owned(),
        path,
        path_origin,
        with_features,
        execution_requirements: string_sequence_attr(tool, "execution_requirements", heap)?,
    })
}

fn parse_tool_paths<'v>(
    toolchain_config_info: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Vec<(String, String)>> {
    let Some(tool_paths) = toolchain_config_info.get_attr("tool_paths", heap)? else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::new();
    for tool_path in sequence_values(tool_paths, "tool_paths")? {
        let Some(name) = string_attr(tool_path, "name", heap)? else {
            continue;
        };
        let Some(path) = string_attr(tool_path, "path", heap)? else {
            continue;
        };
        parsed.push((name, path));
    }
    Ok(parsed)
}

fn artifact_category(category: &str) -> starlark::Result<&'static BazelArtifactCategory> {
    let category = category.to_ascii_lowercase();
    BAZEL_CC_ARTIFACT_CATEGORIES
        .iter()
        .find(|candidate| candidate.name == category)
        .ok_or_else(|| bazel_cc_error(format!("Artifact category {category} not recognized.")))
}

fn parse_artifact_name_patterns<'v>(
    toolchain_config_info: Value<'v>,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<Vec<BazelArtifactNamePattern>> {
    let Some(patterns) =
        toolchain_config_info.get_attr("_artifact_name_patterns_DO_NOT_USE", heap)?
    else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::new();
    for pattern in sequence_values(patterns, "_artifact_name_patterns_DO_NOT_USE")? {
        let category_name = string_attr(pattern, "category_name", heap)?.ok_or_else(|| {
            bazel_cc_error("The `category_name` field of artifact_name_pattern must be a string.")
        })?;
        if category_name.is_empty() {
            return Err(bazel_cc_error(
                "The `category_name` field of artifact_name_pattern must be a nonempty string.",
            ));
        }
        let category = artifact_category(&category_name)?;
        let prefix = string_attr(pattern, "prefix", heap)?.unwrap_or_default();
        let extension = string_attr(pattern, "extension", heap)?.unwrap_or_default();
        if !category.allowed_extensions.contains(&extension.as_str()) {
            return Err(bazel_cc_error(format!(
                "Unrecognized file extension `{extension}` for artifact category `{}`.",
                category.name
            )));
        }
        if parsed
            .iter()
            .any(|existing: &BazelArtifactNamePattern| existing.category == category.name)
        {
            return Err(bazel_cc_error(format!(
                "Duplicate artifact_name_pattern for category `{}`.",
                category.name
            )));
        }
        if prefix != category.default_prefix || extension != category.default_extension {
            parsed.push(BazelArtifactNamePattern {
                category: category.name.to_owned(),
                prefix,
                extension,
            });
        }
    }

    Ok(parsed)
}

fn tool_path_origin(path: &str) -> BazelToolPathOrigin {
    if path.starts_with('/') {
        BazelToolPathOrigin::FilesystemRoot
    } else {
        BazelToolPathOrigin::CrosstoolPackage
    }
}

fn legacy_action_tool(action_name: &str, path: &str) -> BazelActionTool {
    BazelActionTool {
        action_name: action_name.to_owned(),
        path: path.to_owned(),
        path_origin: tool_path_origin(path),
        with_features: Vec::new(),
        execution_requirements: Vec::new(),
    }
}

fn add_legacy_action_config(
    selectables: &mut Vec<BazelSelectable>,
    action_tools: &mut Vec<BazelActionTool>,
    action_name: &str,
    tool_path: Option<&str>,
) {
    let Some(tool_path) = tool_path else {
        return;
    };
    selectables.push(BazelSelectable {
        name: action_name.to_owned(),
        requires: Vec::new(),
        implies: Vec::new(),
        provides: Vec::new(),
    });
    action_tools.push(legacy_action_tool(action_name, tool_path));
}

fn add_legacy_action_configs(
    selectables: &mut Vec<BazelSelectable>,
    action_tools: &mut Vec<BazelActionTool>,
    tool_paths: &[(String, String)],
) {
    let tool_path = |name: &str| {
        tool_paths
            .iter()
            .find_map(|(tool_name, path)| (tool_name == name).then_some(path.as_str()))
    };

    let gcc = tool_path("gcc");
    for action_name in [
        "assemble",
        "preprocess-assemble",
        "linkstamp-compile",
        "lto-backend",
        "c-compile",
        "c++-compile",
        "c++-header-parsing",
        "c++-module-compile",
        "c++-module-codegen",
        "c++-link-executable",
        "lto-index-for-executable",
        "c++-link-nodeps-dynamic-library",
        "lto-index-for-nodeps-dynamic-library",
        "c++-link-dynamic-library",
        "lto-index-for-dynamic-library",
    ] {
        add_legacy_action_config(selectables, action_tools, action_name, gcc);
    }
    add_legacy_action_config(
        selectables,
        action_tools,
        "c++-link-static-library",
        tool_path("ar"),
    );
    add_legacy_action_config(selectables, action_tools, "strip", tool_path("strip"));
}

fn parse_toolchain_features<'v>(
    toolchain_config_info: Value<'v>,
    tools_directory: String,
    heap: starlark::values::Heap<'v>,
) -> starlark::Result<BazelCcToolchainFeatures> {
    let mut selectables = Vec::new();
    let mut default_selectables = Vec::new();
    let mut action_tools = Vec::new();

    if let Some(features) = toolchain_config_info.get_attr("_features_DO_NOT_USE", heap)? {
        for feature in sequence_values(features, "_features_DO_NOT_USE")? {
            let Some(name) = string_attr(feature, "name", heap)? else {
                continue;
            };
            let enabled = bool_attr(feature, "enabled", heap)?;
            if enabled {
                push_unique(&mut default_selectables, name.clone());
            }
            selectables.push(BazelSelectable {
                name,
                requires: parse_requires(feature, heap)?,
                implies: string_sequence_attr(feature, "implies", heap)?,
                provides: string_sequence_attr(feature, "provides", heap)?,
            });
        }
    }

    if let Some(action_configs) =
        toolchain_config_info.get_attr("_action_configs_DO_NOT_USE", heap)?
    {
        for action_config in sequence_values(action_configs, "_action_configs_DO_NOT_USE")? {
            let Some(action_name) = string_attr(action_config, "action_name", heap)? else {
                continue;
            };
            let enabled = bool_attr(action_config, "enabled", heap)?;
            if enabled {
                push_unique(&mut default_selectables, action_name.clone());
            }
            selectables.push(BazelSelectable {
                name: action_name.clone(),
                requires: Vec::new(),
                implies: string_sequence_attr(action_config, "implies", heap)?,
                provides: Vec::new(),
            });

            if let Some(tools) = action_config.get_attr("tools", heap)? {
                for tool in sequence_values(tools, "tools")? {
                    action_tools.push(parse_tool(&action_name, tool, heap)?);
                }
            }
        }
    }

    if action_tools.is_empty() {
        let tool_paths = parse_tool_paths(toolchain_config_info, heap)?;
        add_legacy_action_configs(&mut selectables, &mut action_tools, &tool_paths);
    }

    let artifact_name_patterns = parse_artifact_name_patterns(toolchain_config_info, heap)?;

    validate_selectables(&selectables)?;

    Ok(BazelCcToolchainFeatures {
        selectables,
        default_selectables,
        action_tools,
        artifact_name_patterns,
        tools_directory,
    })
}

fn enabled_selectables(
    selectables: &[BazelSelectable],
    requested_features: &[String],
) -> starlark::Result<Vec<String>> {
    let mut enabled = Vec::new();
    let mut requested = Vec::new();
    for requested_feature in requested_features {
        if let Some(index) = selectable_index(selectables, requested_feature) {
            push_unique_index(&mut requested, index);
            enable_all_implied_by(selectables, &mut enabled, index);
        }
    }

    loop {
        let mut changed = false;
        for index in 0..selectables.len() {
            if !enabled.contains(&index) {
                continue;
            }
            if !is_selectable_satisfied(selectables, &enabled, &requested, index) {
                enabled.retain(|enabled_index| *enabled_index != index);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    check_provides_conflicts(selectables, &enabled)?;

    Ok(selectables
        .iter()
        .enumerate()
        .filter_map(|(index, selectable)| enabled.contains(&index).then(|| selectable.name.clone()))
        .collect())
}

fn selectable_index(selectables: &[BazelSelectable], name: &str) -> Option<usize> {
    selectables
        .iter()
        .position(|selectable| selectable.name == name)
}

fn push_unique_index(values: &mut Vec<usize>, value: usize) -> bool {
    if values.contains(&value) {
        false
    } else {
        values.push(value);
        true
    }
}

fn enable_all_implied_by(selectables: &[BazelSelectable], enabled: &mut Vec<usize>, index: usize) {
    if !push_unique_index(enabled, index) {
        return;
    }
    for implied in &selectables[index].implies {
        if let Some(implied_index) = selectable_index(selectables, implied) {
            enable_all_implied_by(selectables, enabled, implied_index);
        }
    }
}

fn is_selectable_satisfied(
    selectables: &[BazelSelectable],
    enabled: &[usize],
    requested: &[usize],
    index: usize,
) -> bool {
    (requested.contains(&index)
        || selectables
            .iter()
            .enumerate()
            .any(|(other_index, selectable)| {
                enabled.contains(&other_index)
                    && selectable
                        .implies
                        .iter()
                        .any(|implied| implied == &selectables[index].name)
            }))
        && selectables[index].implies.iter().all(|implied| {
            selectable_index(selectables, implied)
                .is_some_and(|implied_index| enabled.contains(&implied_index))
        })
        && (selectables[index].requires.is_empty()
            || selectables[index].requires.iter().any(|required_set| {
                required_set.iter().all(|required| {
                    selectable_index(selectables, required)
                        .is_some_and(|required_index| enabled.contains(&required_index))
                })
            }))
}

fn validate_selectables(selectables: &[BazelSelectable]) -> starlark::Result<()> {
    for (index, selectable) in selectables.iter().enumerate() {
        if selectables[..index]
            .iter()
            .any(|existing| existing.name == selectable.name)
        {
            return Err(bazel_cc_error(format!(
                "Invalid toolchain configuration: feature or action config '{}' was specified multiple times.",
                selectable.name
            )));
        }
        for implied in &selectable.implies {
            if selectable_index(selectables, implied).is_none() {
                return Err(bazel_cc_error(format!(
                    "Invalid toolchain configuration: '{}' implies unknown feature or action config '{}'.",
                    selectable.name, implied
                )));
            }
        }
        for required_set in &selectable.requires {
            for required in required_set {
                if selectable_index(selectables, required).is_none() {
                    return Err(bazel_cc_error(format!(
                        "Invalid toolchain configuration: '{}' requires unknown feature or action config '{}'.",
                        selectable.name, required
                    )));
                }
            }
        }
    }
    Ok(())
}

fn check_provides_conflicts(
    selectables: &[BazelSelectable],
    enabled: &[usize],
) -> starlark::Result<()> {
    let mut provided = Vec::<(&str, &str)>::new();
    for index in enabled {
        let selectable = &selectables[*index];
        for provides in &selectable.provides {
            if let Some((_, existing)) = provided
                .iter()
                .find(|(provided_name, _)| *provided_name == provides.as_str())
            {
                return Err(bazel_cc_error(format!(
                    "Invalid toolchain configuration: features '{}' and '{}' both provide '{}'.",
                    existing, selectable.name, provides
                )));
            }
            provided.push((provides, selectable.name.as_str()));
        }
    }
    Ok(())
}

impl BazelWithFeatureSet {
    fn matches(&self, enabled: &[String]) -> bool {
        self.features
            .iter()
            .all(|feature| enabled.iter().any(|enabled| enabled == feature))
            && self
                .not_features
                .iter()
                .all(|feature| !enabled.iter().any(|enabled| enabled == feature))
    }
}

impl BazelActionTool {
    fn matches(&self, enabled: &[String]) -> bool {
        self.with_features.is_empty()
            || self
                .with_features
                .iter()
                .any(|with_features| with_features.matches(enabled))
    }

    fn tool_path(&self, tools_directory: &str) -> String {
        match self.path_origin {
            BazelToolPathOrigin::FilesystemRoot | BazelToolPathOrigin::WorkspaceRoot => {
                self.path.clone()
            }
            BazelToolPathOrigin::CrosstoolPackage => {
                if tools_directory.is_empty() || self.path.starts_with('/') {
                    self.path.clone()
                } else {
                    format!(
                        "{}/{}",
                        tools_directory.trim_end_matches('/'),
                        self.path.trim_start_matches('/')
                    )
                }
            }
        }
    }
}

fn toolchain_features_from_toolchain<'v>(
    cc_toolchain: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<&'v BazelCcToolchainFeatures> {
    let Some(toolchain_features) = cc_toolchain.get_attr("_toolchain_features", heap)? else {
        return Err(bazel_cc_error(
            "Expected cc_toolchain to expose a `_toolchain_features` attribute",
        ));
    };
    toolchain_features
        .downcast_ref::<BazelCcToolchainFeatures>()
        .ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected cc_toolchain._toolchain_features to be CcToolchainFeatures, got `{}`",
                toolchain_features.get_type()
            ))
        })
}

fn artifact_name_pattern<'a>(
    features: &'a BazelCcToolchainFeatures,
    category: &'static BazelArtifactCategory,
) -> (&'a str, &'a str) {
    features
        .artifact_name_patterns
        .iter()
        .find(|pattern| pattern.category == category.name)
        .map(|pattern| (pattern.prefix.as_str(), pattern.extension.as_str()))
        .unwrap_or((category.default_prefix, category.default_extension))
}

fn artifact_name(output_name: &str, prefix: &str, extension: &str) -> String {
    let artifact_basename = match output_name.rsplit_once('/') {
        Some((parent, basename)) => {
            return format!("{parent}/{prefix}{basename}{extension}");
        }
        None => output_name,
    };
    format!("{prefix}{artifact_basename}{extension}")
}

impl BazelFeatureConfiguration {
    fn is_enabled_selectable(&self, name: &str) -> bool {
        self.enabled_selectables
            .iter()
            .any(|selectable| selectable == name)
    }

    fn action_is_configured(&self, action_name: &str) -> bool {
        self.action_tools
            .iter()
            .any(|tool| tool.action_name == action_name)
    }

    fn selected_tool(&self, action_name: &str) -> starlark::Result<&BazelActionTool> {
        let candidate_count = self
            .action_tools
            .iter()
            .filter(|tool| tool.action_name == action_name)
            .count();
        let known_actions = self
            .action_tools
            .iter()
            .map(|tool| tool.action_name.as_str())
            .take(20)
            .collect::<Vec<_>>()
            .join(", ");
        self.action_tools
            .iter()
            .filter(|tool| tool.action_name == action_name)
            .find(|tool| tool.matches(&self.enabled_selectables))
            .ok_or_else(|| {
                bazel_cc_error(format!(
                    "Matching tool for action {action_name} not found for given feature configuration; candidate tools: {candidate_count}; known action tools: [{known_actions}]"
                ))
            })
    }
}

#[starlark_value(type = "FeatureConfiguration")]
impl<'v> StarlarkValue<'v> for BazelFeatureConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_feature_configuration_methods)
    }
}

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for BazelCcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "check_private_api".to_owned(),
            "cc_toolchain_features".to_owned(),
            "cc_toolchain_variables".to_owned(),
            "combine_cc_toolchain_variables".to_owned(),
            "actions2ctx_cheat".to_owned(),
            "compute_output_name_prefix_dir".to_owned(),
            "create_header_info".to_owned(),
            "create_header_info_with_deps".to_owned(),
            "dynamic_library_soname".to_owned(),
            "exec_os".to_owned(),
            "freeze".to_owned(),
            "get_artifact_name_extension_for_category".to_owned(),
            "get_artifact_name_for_category".to_owned(),
            "intern_seq".to_owned(),
            "intern_string_sequence_variable_value".to_owned(),
            "is_tree_artifact".to_owned(),
            "wrap_link_actions".to_owned(),
        ]
    }
}

fn bazel_cc_escape_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '_' => escaped.push_str("_U"),
            '/' => escaped.push_str("_S"),
            '\\' => escaped.push_str("_B"),
            ':' => escaped.push_str("_C"),
            '@' => escaped.push_str("_A"),
            _ => escaped.push(c),
        }
    }
    escaped
}

fn bazel_cc_dynamic_library_soname(path: &str, preserve_name: bool, mnemonic: &str) -> String {
    if preserve_name {
        return path.rsplit('/').next().unwrap_or(path).to_owned();
    }

    let mnemonic_mangling = mnemonic
        .find("ST-")
        .map(|idx| format!("{}_", &mnemonic[idx..]))
        .unwrap_or_default();
    format!("lib{}{}", mnemonic_mangling, bazel_cc_escape_path(path))
}

fn bazel_cc_exec_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "DARWIN"
    } else if cfg!(target_os = "linux") {
        "LINUX"
    } else if cfg!(target_os = "windows") {
        "WINDOWS"
    } else if cfg!(target_os = "freebsd") {
        "FREEBSD"
    } else if cfg!(target_os = "openbsd") {
        "OPENBSD"
    } else {
        "UNKNOWN"
    }
}

fn bazel_file_root<'v>(heap: Heap<'v>, path: &str) -> Value<'v> {
    heap.alloc(AllocStruct([("path", heap.alloc_str(path).to_value())]))
}

fn kw_value<'v>(kwargs: &SmallMap<String, Value<'v>>, name: &str, default: Value<'v>) -> Value<'v> {
    kwargs.get(name).copied().unwrap_or(default)
}

fn header_info_attr<'v>(
    header_info: Value<'v>,
    name: &str,
    default: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if header_info.is_none() {
        return Ok(default);
    }
    Ok(header_info.get_attr(name, eval.heap())?.unwrap_or(default))
}

fn alloc_header_info<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Value<'v> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    eval.heap().alloc(AllocStruct([
        ("header_module", kw_value(kwargs, "header_module", none)),
        (
            "pic_header_module",
            kw_value(kwargs, "pic_header_module", none),
        ),
        (
            "modular_public_headers",
            kw_value(kwargs, "modular_public_headers", empty_list),
        ),
        (
            "modular_private_headers",
            kw_value(kwargs, "modular_private_headers", empty_list),
        ),
        (
            "textual_headers",
            kw_value(kwargs, "textual_headers", empty_list),
        ),
        (
            "separate_module_headers",
            kw_value(kwargs, "separate_module_headers", empty_list),
        ),
        ("separate_module", kw_value(kwargs, "separate_module", none)),
        (
            "separate_pic_module",
            kw_value(kwargs, "separate_pic_module", none),
        ),
        ("deps", kw_value(kwargs, "deps", empty_list)),
        ("merged_deps", kw_value(kwargs, "merged_deps", empty_list)),
    ]))
}

fn alloc_header_info_with_deps<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    let header_info = kw_value(kwargs, "header_info", none);
    Ok(eval.heap().alloc(AllocStruct([
        (
            "header_module",
            header_info_attr(header_info, "header_module", none, eval)?,
        ),
        (
            "pic_header_module",
            header_info_attr(header_info, "pic_header_module", none, eval)?,
        ),
        (
            "modular_public_headers",
            header_info_attr(header_info, "modular_public_headers", empty_list, eval)?,
        ),
        (
            "modular_private_headers",
            header_info_attr(header_info, "modular_private_headers", empty_list, eval)?,
        ),
        (
            "textual_headers",
            header_info_attr(header_info, "textual_headers", empty_list, eval)?,
        ),
        (
            "separate_module_headers",
            header_info_attr(header_info, "separate_module_headers", empty_list, eval)?,
        ),
        (
            "separate_module",
            header_info_attr(header_info, "separate_module", none, eval)?,
        ),
        (
            "separate_pic_module",
            header_info_attr(header_info, "separate_pic_module", none, eval)?,
        ),
        ("deps", kw_value(kwargs, "deps", empty_list)),
        ("merged_deps", kw_value(kwargs, "merged_deps", empty_list)),
    ])))
}

#[starlark_module]
fn bazel_cc_toolchain_features_methods(builder: &mut MethodsBuilder) {
    fn default_features_and_action_configs<'v>(
        #[starlark(this)] this: &BazelCcToolchainFeatures,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            this.default_selectables
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn configure_features(
        #[starlark(this)] this: &BazelCcToolchainFeatures,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        let requested_features = requested_features.into_iter().collect::<Vec<_>>();
        let enabled_selectables = enabled_selectables(&this.selectables, &requested_features)?;
        Ok(BazelFeatureConfiguration {
            requested_features,
            enabled_selectables,
            action_tools: this.action_tools.clone(),
            tools_directory: this.tools_directory.clone(),
        })
    }
}

#[starlark_module]
fn bazel_feature_configuration_methods(builder: &mut MethodsBuilder) {
    fn is_enabled(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this.is_enabled_selectable(feature))
    }

    fn is_requested(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this
            .requested_features
            .iter()
            .any(|requested| requested == feature))
    }
}

#[starlark_module]
fn bazel_cc_internal_methods(builder: &mut MethodsBuilder) {
    fn cc_toolchain_features<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelCcToolchainFeatures> {
        let heap = eval.heap();
        let toolchain_config_info = kw_value(&kwargs, "toolchain_config_info", Value::new_none());
        let tools_directory = kw_value(&kwargs, "tools_directory", Value::new_none())
            .unpack_str()
            .unwrap_or("")
            .to_owned();
        parse_toolchain_features(toolchain_config_info, tools_directory, heap)
    }

    fn cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] vars: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(vars)
    }

    fn combine_cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        parent: Value<'v>,
        #[starlark(args)] _variables: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if parent.is_none() {
            Ok(eval.heap().alloc(AllocDict::EMPTY))
        } else {
            Ok(parent)
        }
    }

    fn intern_string_sequence_variable_value<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        string_sequence: UnpackList<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let values = string_sequence
            .into_iter()
            .map(|value| heap.alloc_str(&value))
            .collect::<Vec<_>>();
        Ok(heap.alloc(AllocTuple(values)))
    }

    fn intern_seq<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        seq: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocTuple(sequence_values(seq, "seq")?)))
    }

    fn compute_output_name_prefix_dir<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] configuration: Value<'v>,
        #[starlark(require = named, default = NoneType)] purpose: Value<'v>,
    ) -> starlark::Result<&'static str> {
        let _unused = configuration;
        let mnemonic = purpose.unpack_str().unwrap_or("");
        if mnemonic.ends_with("_objc_arc") {
            if mnemonic.ends_with("_non_objc_arc") {
                Ok("non_arc")
            } else {
                Ok("arc")
            }
        } else {
            Ok("")
        }
    }

    fn is_tree_artifact<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        artifact: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let Some(is_directory) = artifact.get_attr("is_directory", eval.heap())? else {
            return Ok(false);
        };
        is_directory.unpack_bool().ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected artifact.is_directory to be a bool, got `{}`",
                is_directory.get_type()
            ))
        })
    }

    fn get_artifact_name_for_category<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        #[starlark(require = named)] output_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let category = artifact_category(category)?;
        let features = toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        let (prefix, extension) = artifact_name_pattern(features, category);
        Ok(artifact_name(output_name, prefix, extension))
    }

    fn get_artifact_name_extension_for_category<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let category = artifact_category(category)?;
        let features = toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        let (_, extension) = artifact_name_pattern(features, category);
        Ok(extension.to_owned())
    }

    fn wrap_link_actions<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: Value<'v>,
        #[starlark(default = NoneType)] build_configuration: Value<'v>,
        #[starlark(default = false)] sharable_artifacts: bool,
    ) -> starlark::Result<Value<'v>> {
        let _unused = (build_configuration, sharable_artifacts);
        Ok(actions)
    }

    fn actions2ctx_cheat<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let empty_struct = heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()));
        let get_attr = |name: &str, default: Value<'v>| -> starlark::Result<Value<'v>> {
            Ok(actions.get_attr(name, heap)?.unwrap_or(default))
        };
        Ok(heap.alloc(AllocStruct([
            ("actions", actions),
            ("attr", get_attr("attr", empty_struct)?),
            ("attrs", get_attr("attrs", empty_struct)?),
            (
                "bin_dir",
                get_attr("bin_dir", bazel_file_root(heap, "buck-out/bin"))?,
            ),
            ("configuration", get_attr("configuration", empty_struct)?),
            (
                "disabled_features",
                get_attr("disabled_features", heap.alloc(AllocList::EMPTY))?,
            ),
            (
                "exec_groups",
                get_attr("exec_groups", heap.alloc(AllocDict::EMPTY))?,
            ),
            ("executable", empty_struct),
            (
                "features",
                get_attr("features", heap.alloc(AllocList::EMPTY))?,
            ),
            ("file", empty_struct),
            ("files", empty_struct),
            ("fragments", get_attr("fragments", empty_struct)?),
            (
                "genfiles_dir",
                get_attr("genfiles_dir", bazel_file_root(heap, "buck-out/genfiles"))?,
            ),
            ("info_file", get_attr("info_file", Value::new_none())?),
            ("label", get_attr("label", Value::new_none())?),
            ("outputs", empty_struct),
            (
                "toolchains",
                get_attr("toolchains", heap.alloc(AllocDict::EMPTY))?,
            ),
            ("version_file", get_attr("version_file", Value::new_none())?),
            (
                "workspace_name",
                get_attr("workspace_name", heap.alloc_str("_main").to_value())?,
            ),
        ])))
    }

    fn create_header_info<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(alloc_header_info(&kwargs, eval))
    }

    fn create_header_info_with_deps<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        alloc_header_info_with_deps(&kwargs, eval)
    }

    fn dynamic_library_soname<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: Value<'v>,
        path: &str,
        preserve_name: bool,
    ) -> starlark::Result<String> {
        let _unused = actions;
        Ok(bazel_cc_dynamic_library_soname(path, preserve_name, ""))
    }

    fn exec_os<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        ctx: Value<'v>,
    ) -> starlark::Result<&'static str> {
        let _unused = ctx;
        Ok(bazel_cc_exec_os())
    }

    fn freeze<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        value: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(value)
    }

    fn check_private_api<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(args)] _args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }
}

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn internal_DO_NOT_USE() -> starlark::Result<BazelCcInternal> {
        Ok(BazelCcInternal)
    }

    fn configure_features<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named, default = NoneType)] language: Value<'v>,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
        #[starlark(require = named, default = UnpackList::default())]
        unsupported_features: UnpackList<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        let _unused = (ctx, language);
        let unsupported_features = unsupported_features.into_iter().collect::<Vec<_>>();
        let mut requested_features = requested_features
            .into_iter()
            .filter(|feature| !unsupported_features.contains(feature))
            .collect::<Vec<_>>();
        requested_features.sort();
        requested_features.dedup();
        let features = toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        let enabled_selectables = enabled_selectables(&features.selectables, &requested_features)?;
        Ok(BazelFeatureConfiguration {
            requested_features,
            enabled_selectables,
            action_tools: features.action_tools.clone(),
            tools_directory: features.tools_directory.clone(),
        })
    }

    fn get_tool_for_action<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
    ) -> starlark::Result<String> {
        Ok(feature_configuration
            .selected_tool(action_name)?
            .tool_path(&feature_configuration.tools_directory))
    }

    fn get_execution_requirements<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            feature_configuration
                .selected_tool(action_name)?
                .execution_requirements
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn action_is_enabled<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
    ) -> starlark::Result<bool> {
        Ok(feature_configuration.action_is_configured(action_name))
    }

    fn get_memory_inefficient_command_line<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn get_environment_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn empty_variables<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn create_compile_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn create_link_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn legacy_cc_flags_make_variable_do_not_use<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn incompatible_disable_objc_library_transition() -> starlark::Result<bool> {
        Ok(false)
    }

    fn add_go_exec_groups_to_binary_rules() -> starlark::Result<bool> {
        Ok(false)
    }

    fn check_experimental_cc_shared_library() -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_tool_requirement_for_action<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            feature_configuration
                .selected_tool(action_name)?
                .execution_requirements
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn implementation_deps_allowed_by_allowlist<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }
}

pub(crate) fn register_bazel_cc_common(builder: &mut GlobalsBuilder) {
    builder.namespace("cc_common", |cc_common| {
        cc_common.set("do_not_use_tools_cpp_compiler_present", NoneType);
        bazel_cc_common_module(cc_common);
    });
}
