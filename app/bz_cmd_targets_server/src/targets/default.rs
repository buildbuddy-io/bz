/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Server-side implementation of `bz targets` command
//! without `--streaming` or `--resolve-alias` arguments.

use std::io::Write;
use std::path::Path;

use bz_cli_proto::TargetsResponse;
use bz_cli_proto::targets_request;
use bz_cli_proto::targets_request::TargetHashFileMode;
use bz_cli_proto::targets_request::TargetHashGraphType;
use bz_core::cells::CellResolver;
use bz_core::fs::project::ProjectRoot;
use bz_core::global_cfg_options::GlobalCfgOptions;
use bz_core::pattern::pattern::ParsedPattern;
use bz_core::pattern::pattern_type::TargetPatternExtra;
use bz_fs::paths::abs_path::AbsPath;
use bz_node::load_patterns::MissingTargetBehavior;
use bz_node::load_patterns::load_patterns;
use bz_node::nodes::configured::ConfiguredTargetNode;
use bz_node::nodes::lookup::ConfiguredTargetNodeLookup;
use bz_node::nodes::lookup::TargetNodeLookup;
use bz_node::nodes::unconfigured::TargetNode;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use dice::DiceTransaction;
use dupe::Dupe;
use dupe::OptionDupedExt;

use crate::target_hash::TargetHashes;
use crate::target_hash::TargetHashesFileMode;
use crate::targets::fmt::Stats;
use crate::targets::fmt::TargetFormatter;
use crate::targets::fmt::TargetInfo;

pub(crate) struct TargetHashOptions {
    file_mode: TargetHashesFileMode,
    fast_hash: bool,
    graph_type: TargetHashGraphType,
    recursive: bool,
}

impl TargetHashOptions {
    pub(crate) fn new(
        request: &targets_request::Other,
        cell_resolver: &CellResolver,
        fs: &ProjectRoot,
    ) -> bz_error::Result<Self> {
        let file_mode = TargetHashFileMode::try_from(request.target_hash_file_mode)
            .expect("buck cli should send valid target hash file mode");
        let file_mode = match file_mode {
            TargetHashFileMode::PathsOnly => {
                let modified_paths = request
                    .target_hash_modified_paths
                    .iter()
                    .map(|path| {
                        let path = AbsPath::new(Path::new(&path))?;
                        cell_resolver.get_cell_path_from_abs_path(path, fs)
                    })
                    .collect::<bz_error::Result<_>>()?;
                TargetHashesFileMode::PathsOnly(modified_paths)
            }
            TargetHashFileMode::PathsAndContents => TargetHashesFileMode::PathsAndContents,
            TargetHashFileMode::NoFiles => TargetHashesFileMode::None,
        };

        Ok(Self {
            file_mode,
            fast_hash: request.target_hash_use_fast_hash,
            graph_type: TargetHashGraphType::try_from(request.target_hash_graph_type)
                .expect("buck cli should send valid target hash graph type"),
            recursive: request.target_hash_recursive,
        })
    }
}

pub(crate) async fn targets_batch(
    server_ctx: &dyn ServerCommandContextTrait,
    mut dice: DiceTransaction,
    formatter: &dyn TargetFormatter,
    parsed_patterns: Vec<ParsedPattern<TargetPatternExtra>>,
    global_cfg_options: &GlobalCfgOptions,
    hash_options: TargetHashOptions,
    keep_going: bool,
) -> bz_error::Result<TargetsResponse> {
    let results = &load_patterns(&mut dice, parsed_patterns, MissingTargetBehavior::Fail).await?;

    let target_hashes = dice
        .dupe()
        .with_linear_recompute(|linear_ctx| async move {
            match hash_options.graph_type {
                TargetHashGraphType::Configured => bz_error::Ok(Some(
                    TargetHashes::compute::<ConfiguredTargetNode, _>(
                        dice.dupe(),
                        ConfiguredTargetNodeLookup(&linear_ctx),
                        results.iter_loaded_targets_by_package().collect(),
                        global_cfg_options,
                        hash_options.file_mode,
                        hash_options.fast_hash,
                        hash_options.recursive,
                    )
                    .await?,
                )),
                TargetHashGraphType::Unconfigured => Ok(Some(
                    TargetHashes::compute::<TargetNode, _>(
                        dice.dupe(),
                        TargetNodeLookup(&linear_ctx),
                        results.iter_loaded_targets_by_package().collect(),
                        global_cfg_options,
                        hash_options.file_mode,
                        hash_options.fast_hash,
                        hash_options.recursive,
                    )
                    .await?,
                )),
                _ => Ok(None),
            }
        })
        .await?;

    let mut buffer = String::new();
    formatter.begin(&mut buffer);
    let mut stats = Stats::default();
    let mut needs_separator = false;
    for (package_with_modifiers, result) in results.iter() {
        match result {
            Ok(res) => {
                stats.success += 1;
                for (_, node) in res.iter() {
                    stats.targets += 1;
                    let target_hash = target_hashes
                        .as_ref()
                        .and_then(|hashes| hashes.get(node.label()))
                        .duped()
                        .transpose()?;
                    if needs_separator {
                        formatter.separator(&mut buffer);
                    }
                    needs_separator = true;
                    formatter.target(
                        TargetInfo {
                            node,
                            target_hash,
                            super_package: res.super_package(),
                        },
                        &mut buffer,
                    )
                }
            }
            Err(e) => {
                stats.add_error(e);
                let mut stderr = String::new();

                if needs_separator {
                    formatter.separator(&mut buffer);
                }
                needs_separator = true;
                formatter.package_error(
                    package_with_modifiers.package.dupe(),
                    e,
                    &mut buffer,
                    &mut stderr,
                );

                server_ctx.stderr()?.write_all(stderr.as_bytes())?;

                if !keep_going {
                    break;
                }
            }
        }
    }
    formatter.end(&stats, &mut buffer);
    if !keep_going && let Some(e) = stats.to_error() {
        Err(e)
    } else {
        Ok(TargetsResponse {
            error_count: stats.errors,
            serialized_targets_output: buffer,
        })
    }
}
