/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Processing and reporting the the results of the build

use std::cell::Cell;
use std::collections::BTreeSet;

use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::BaseArtifactKind;
use bz_build_api::build::BuildProviderType;
use bz_build_api::build::BuildTargetResult;
use bz_build_api::build::ConfiguredBuildTargetResult;
use bz_build_api::build::ProviderArtifacts;
use bz_build_api::interpreter::rule_defs::cmd_args::AbsCommandLineContext;
use bz_build_api::interpreter::rule_defs::cmd_args::ArtifactPathMapper;
use bz_build_api::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use bz_build_api::interpreter::rule_defs::context::bazel_workspace_name_for_cell;
use bz_build_api::interpreter::rule_defs::provider::builtin::bazel::run_environment_info::FrozenRunEnvironmentInfo;
use bz_build_api::interpreter::rule_defs::provider::builtin::default_info::bazel_files_to_run_add_executable_to_command_line;
use bz_build_api::interpreter::rule_defs::provider::builtin::run_info::FrozenRunInfo;
use bz_certs::validate::CertState;
use bz_certs::validate::check_cert_state;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::content_hash::ContentBasedPathHash;
use bz_core::execution_types::executor_config::PathSeparatorKind;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::buck_out_path::BazelOutputPathKind;
use bz_core::pattern::pattern::Modifiers;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_execute::artifact::artifact_dyn::ArtifactDyn;
use bz_execute::artifact::fs::ExecutorFs;
use bz_hash::BuckHashMap;
use starlark_map::small_map::SmallMap;

use crate::build::bazel_output_symlinks::BazelOutputSymlinkPaths;

mod proto {
    pub(crate) use bz_cli_proto::BuildTarget;
    pub(crate) use bz_cli_proto::ClientEnvironmentVariable;
    pub(crate) use bz_cli_proto::build_target::BuildOutput;
    pub(crate) use bz_cli_proto::build_target::RunSpec;
    pub(crate) use bz_cli_proto::build_target::build_output::BuildOutputProviders;
    pub(crate) use bz_cli_proto::build_target::run_spec::Runfile;
}

/// Simple container for multiple [`bz_error::Error`]s
pub(crate) struct BuildErrors {
    pub(crate) errors: Vec<bz_error::Error>,
}

#[derive(Clone, Default)]
pub(crate) struct ResultReporterOptions {
    pub(crate) return_outputs: bool,
    pub(crate) bazel_output_symlinks: BazelOutputSymlinkPaths,
}

/// Collects build results into a Result<Vec<proto::BuildTarget>, bz_error::Errors>. If any targets
/// fail, then the error case will be returned, otherwise a vec of all the successful results.
pub(crate) struct ResultReporter<'a> {
    artifact_fs: &'a ArtifactFs,
    options: ResultReporterOptions,
    results: Vec<proto::BuildTarget>,
}

pub(crate) struct BuildTargetsAndErrors {
    pub(crate) build_targets: Vec<proto::BuildTarget>,
    pub(crate) build_errors: BuildErrors,
}

impl<'a> ResultReporter<'a> {
    pub(crate) async fn convert(
        artifact_fs: &'a ArtifactFs,
        cert_state: CertState,
        options: ResultReporterOptions,
        build_result: &BuildTargetResult,
    ) -> bz_error::Result<BuildTargetsAndErrors> {
        let mut out = Self {
            artifact_fs,
            options,
            results: Vec::new(),
        };

        let mut non_action_errors = vec![];
        let mut action_errors = vec![];
        non_action_errors.extend(build_result.other_errors.values().flatten().cloned());

        for (k, v) in &build_result.configured {
            // We omit skipped targets here.
            let Some(v) = v else { continue };
            non_action_errors.extend(v.errors.iter().map(|t| t.inner.clone()));
            action_errors.extend(
                v.outputs
                    .iter()
                    .filter_map(|x| x.inner.as_ref().err())
                    .cloned(),
            );

            out.collect_result(k, v, build_result.configured_to_pattern_modifiers.get(k))?;
        }

        let mut error_list = if let Some(e) = non_action_errors.pop() {
            // FIXME(JakobDegen): We'd like to return more than one error here, but we have
            // to get better at error deduplication first
            vec![e]
        } else {
            // FIXME: Only one non-action error or all action errors is returned currently
            action_errors
        };

        if !error_list.is_empty() {
            if let Some(e) = check_cert_state(cert_state).await {
                error_list.push(e);
            }
        }

        Ok(BuildTargetsAndErrors {
            build_targets: out.results,
            build_errors: BuildErrors { errors: error_list },
        })
    }

    fn collect_result(
        &mut self,
        label: &ConfiguredProvidersLabel,
        result: &ConfiguredBuildTargetResult,
        pattern_modifiers: Option<&BTreeSet<Modifiers>>,
    ) -> bz_error::Result<()> {
        let outputs = result
            .outputs
            .iter()
            .filter_map(|output| output.inner.as_ref().ok());

        let mut artifact_path_mapping = BuckHashMap::default();

        // NOTE: We use an SmallMap here to preserve the order the rule author wrote, all
        // the while avoiding duplicates.
        let mut artifacts = SmallMap::new();

        for output in outputs {
            let ProviderArtifacts {
                values,
                provider_type,
            } = output;

            if !self.options.return_outputs && !matches!(provider_type, BuildProviderType::Run) {
                continue;
            }

            if matches!(provider_type, BuildProviderType::DefaultOther) {
                continue;
            }

            for (artifact, value) in values.iter() {
                if matches!(provider_type, BuildProviderType::Run) {
                    artifact_path_mapping.insert(artifact, value.content_based_path_hash());
                }

                if self.options.return_outputs {
                    let entry =
                        artifacts
                            .entry(artifact)
                            .or_insert_with(|| proto::BuildOutputProviders {
                                default_info: false,
                                run_info: false,
                                other: false,
                                test_info: false,
                            });

                    match provider_type {
                        BuildProviderType::Default => {
                            entry.default_info = true;
                        }
                        BuildProviderType::DefaultOther => {
                            entry.other = true;
                        }
                        BuildProviderType::Run => {
                            entry.run_info = true;
                        }
                        BuildProviderType::Test => {
                            entry.test_info = true;
                        }
                    }
                }
            }
        }

        let artifact_fs = self.artifact_fs;

        let mut outputs: Vec<proto::BuildOutput> = Vec::new();
        for (a, providers) in artifacts.into_iter() {
            let output = proto::BuildOutput {
                path: build_output_display_path(a, artifact_fs, &self.options)?,
                providers: Some(providers),
            };
            outputs.push(output);
        }

        let target = label.unconfigured().to_string();
        let configuration = label.cfg().to_string();

        let configured_graph_size = match &result.graph_properties {
            Some(Ok(MaybeCompatible::Compatible(v))) => Some(v.configured.configured_graph_size),
            Some(Ok(MaybeCompatible::Incompatible(..))) => None,
            Some(Err(e)) => {
                // We don't expect an error on this unless something else on this target
                // failed.
                tracing::debug!(
                    "Graph size calculation error failed for {}: {:#}",
                    target,
                    e
                );
                None
            }
            None => None,
        };

        let (run_args, run_environment, run_inherited_environment, run_spec) =
            if let Some(providers) = result.provider_collection.as_ref() {
                let provider_collection = providers.provider_collection();
                let (run_environment, run_inherited_environment): (
                    Vec<proto::ClientEnvironmentVariable>,
                    Vec<String>,
                ) = if let Some(run_environment_info) =
                    provider_collection.builtin_provider::<FrozenRunEnvironmentInfo>()
                {
                    (
                        run_environment_info
                            .environment()?
                            .into_iter()
                            .map(|(name, value)| proto::ClientEnvironmentVariable {
                                name,
                                value: Some(value),
                            })
                            .collect(),
                        run_environment_info.inherited_environment()?,
                    )
                } else {
                    (
                        result
                            .bazel_run_environment
                            .iter()
                            .map(|(name, value)| proto::ClientEnvironmentVariable {
                                name: name.clone(),
                                value: Some(value.clone()),
                            })
                            .collect(),
                        result.bazel_run_inherited_environment.clone(),
                    )
                };
                let path_separator = if cfg!(windows) {
                    PathSeparatorKind::Windows
                } else {
                    PathSeparatorKind::Unix
                };
                let executor_fs = ExecutorFs::new(self.artifact_fs, path_separator);
                let mut cli = Vec::<String>::new();
                let mut ctx = AbsCommandLineContext::new(&executor_fs);
                let error_counting_artifact_path_mapper =
                    ErrorCountingArtifactPathMapperImpl::new(artifact_path_mapping);
                let mut added_bazel_files_to_run = false;
                let mut run_spec: Option<proto::RunSpec> = None;
                if let Ok(default_info) = provider_collection.default_info() {
                    // Bazel executable rules expose DefaultInfo.files_to_run instead of Buck RunInfo.
                    added_bazel_files_to_run = bazel_files_to_run_add_executable_to_command_line(
                        default_info.files_to_run_raw().to_value(),
                        &mut cli,
                        &mut ctx,
                        &error_counting_artifact_path_mapper,
                    )?;
                    if added_bazel_files_to_run && let Some(executable) = cli.first() {
                        let mut runfiles = Vec::new();
                        default_info.for_each_default_runfiles_entry(&mut |path, artifact| {
                            runfiles.push(proto::Runfile {
                                path,
                                target_path: artifact
                                    .resolve_path(
                                        self.artifact_fs,
                                        error_counting_artifact_path_mapper.get(&artifact),
                                    )?
                                    .to_string(),
                            });
                            Ok(())
                        })?;
                        let workspace_name = bazel_workspace_name_for_cell(
                            label.target().pkg().cell_name().as_str(),
                        );
                        let runfiles_dir = format!("{executable}.runfiles");
                        let working_directory = if workspace_name.is_empty() {
                            runfiles_dir.clone()
                        } else {
                            format!("{runfiles_dir}/{workspace_name}")
                        };
                        let execroot = format!(
                            "{}/__bazel_execroot",
                            self.artifact_fs.buck_out_path_resolver().root()
                        );
                        run_spec = Some(proto::RunSpec {
                            executable: executable.clone(),
                            target_args: result.bazel_target_args.clone(),
                            runfiles_dir: runfiles_dir.clone(),
                            working_directory,
                            environment: run_environment.clone(),
                            inherited_environment: run_inherited_environment.clone(),
                            environment_to_clear: Vec::new(),
                            execroot,
                            runfiles,
                            empty_filenames: default_info.default_runfiles_empty_filenames()?,
                            workspace_name,
                            bazel_files_to_run: true,
                        });
                    }
                }
                if !added_bazel_files_to_run
                    && let Some(runinfo) = provider_collection.builtin_provider::<FrozenRunInfo>()
                {
                    // Produce arguments to run on a local machine.
                    runinfo.add_to_command_line(
                        &mut cli,
                        &mut ctx,
                        &error_counting_artifact_path_mapper,
                    )?;
                }
                let run_args = if error_counting_artifact_path_mapper
                    .content_based_paths_with_no_hash
                    .get()
                    > 0
                {
                    // If we have action errors, then it's possible that we weren't able to produce
                    // the run info because we couldn't resolve a content-based path, and that's okay
                    // because we don't expect to be able to use it anyway.
                    Vec::new()
                } else {
                    cli
                };
                (
                    run_args,
                    run_environment,
                    run_inherited_environment,
                    run_spec,
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new(), None)
            };

        match pattern_modifiers {
            Some(modifiers) => {
                for modifier in modifiers.iter() {
                    let target_with_modifiers = match modifier.as_slice() {
                        Some(modifiers) => format!("{}?{}", target, modifiers.join("+")),
                        None => target.clone(),
                    };

                    self.results.push(proto::BuildTarget {
                        target: target_with_modifiers,
                        configuration: configuration.clone(),
                        run_args: run_args.clone(),
                        target_rule_type_name: result.target_rule_type_name.clone(),
                        outputs: outputs.clone(),
                        configured_graph_size,
                        run_environment: run_environment.clone(),
                        run_inherited_environment: run_inherited_environment.clone(),
                        run: run_spec.clone(),
                    });
                }
            }
            None => self.results.push(proto::BuildTarget {
                target,
                configuration,
                run_args,
                target_rule_type_name: result.target_rule_type_name.clone(),
                outputs,
                configured_graph_size,
                run_environment,
                run_inherited_environment,
                run: run_spec,
            }),
        }
        Ok(())
    }
}

fn build_output_display_path(
    artifact: &Artifact,
    artifact_fs: &ArtifactFs,
    options: &ResultReporterOptions,
) -> bz_error::Result<String> {
    if let Some(path) = bazel_convenience_output_path(artifact, artifact_fs, options) {
        return Ok(path);
    }

    Ok(artifact
        .resolve_configuration_hash_path(artifact_fs)?
        .to_string())
}

fn bazel_convenience_output_path(
    artifact: &Artifact,
    artifact_fs: &ArtifactFs,
    options: &ResultReporterOptions,
) -> Option<String> {
    let (BaseArtifactKind::Build(build), _) = artifact.as_parts() else {
        return None;
    };
    let path = build.get_path();
    let label = path.bazel_owner()?;
    let expected_root = artifact_fs
        .buck_out_path_resolver()
        .resolve_bazel_physical_output_root(label);
    if options
        .bazel_output_symlinks
        .path_for(path.bazel_output_root())?
        != expected_root
    {
        return None;
    }

    let short_path = bazel_visible_build_artifact_short_path(artifact);
    let mut result = path.bazel_output_root().exec_root().to_owned();
    if path.bazel_output_path_kind() == BazelOutputPathKind::PackageRelative {
        let package_exec_path = bazel_package_exec_path(label);
        if !bazel_path_has_package_prefix(&short_path, &package_exec_path) {
            push_bazel_path_component(&mut result, &package_exec_path);
        }
    }
    push_bazel_path_component(&mut result, &short_path);
    Some(result)
}

fn bazel_visible_build_artifact_short_path(artifact: &Artifact) -> String {
    artifact.get_path().with_short_path(|path| {
        let mut result = String::new();
        for component in path
            .iter()
            .filter(|component| !bazel_hidden_path_component(component.as_str()))
        {
            push_bazel_path_component(&mut result, component.as_str());
        }
        result
    })
}

fn bazel_hidden_path_component(component: &str) -> bool {
    component.starts_with("__") && component.ends_with("__")
}

fn bazel_path_has_package_prefix(path: &str, package_exec_path: &str) -> bool {
    !package_exec_path.is_empty()
        && (path == package_exec_path
            || path
                .strip_prefix(package_exec_path)
                .is_some_and(|path| path.starts_with('/')))
}

fn bazel_package_exec_path(label: &ConfiguredTargetLabel) -> String {
    let package = label.pkg();
    let cell = package.cell_name();
    let package_path = package.cell_relative_path();
    let mut path = String::new();
    if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
        push_bazel_path_component(&mut path, "external");
        push_bazel_path_component(&mut path, bazel_external_repo_name(cell.as_str(), &origin));
    } else if !bazel_cell_is_main(cell.as_str()) {
        push_bazel_path_component(&mut path, cell.as_str());
    }
    push_bazel_path_component(&mut path, package_path.as_str());
    path
}

fn bazel_cell_is_main(cell: &str) -> bool {
    cell == "root" || bzlmod_canonical_repo_name_for_cell(cell).is_some_and(|repo| repo.is_empty())
}

fn bazel_external_repo_name<'a>(cell: &'a str, origin: &'a ExternalCellOrigin) -> &'a str {
    match origin {
        ExternalCellOrigin::Bundled(cell) => cell.as_str(),
        ExternalCellOrigin::Git(_) => cell,
        ExternalCellOrigin::Bzlmod(setup) => setup.canonical_repo_name.as_ref(),
        ExternalCellOrigin::BzlmodGenerated(setup) => setup.canonical_repo_name.as_ref(),
    }
}

fn push_bazel_path_component(path: &mut String, component: &str) {
    if component.is_empty() {
        return;
    }
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(component);
}

struct ErrorCountingArtifactPathMapperImpl<'a> {
    pub map: BuckHashMap<&'a Artifact, ContentBasedPathHash>,
    pub content_based_paths_with_no_hash: Cell<usize>,
    pub scratch_content_based_path_hash: ContentBasedPathHash,
}

impl<'a> ErrorCountingArtifactPathMapperImpl<'a> {
    pub fn new(map: BuckHashMap<&'a Artifact, ContentBasedPathHash>) -> Self {
        Self {
            map,
            content_based_paths_with_no_hash: Cell::new(0),
            scratch_content_based_path_hash: ContentBasedPathHash::Scratch,
        }
    }
}

impl ArtifactPathMapper for ErrorCountingArtifactPathMapperImpl<'_> {
    fn get(&self, artifact: &Artifact) -> Option<&ContentBasedPathHash> {
        let content_based_path_hash = self.map.get(artifact);
        if artifact.path_resolution_requires_artifact_value() && content_based_path_hash.is_none() {
            self.content_based_paths_with_no_hash
                .set(self.content_based_paths_with_no_hash.get() + 1);
            // We don't have a hash, but we want path resolution to succeed, so we use a scratch hash.
            return Some(&self.scratch_content_based_path_hash);
        }
        content_based_path_hash
    }
}
