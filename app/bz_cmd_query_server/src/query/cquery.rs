/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io::Write;

use async_trait::async_trait;
use bz_build_api::analysis::calculation::RuleAnalysisCalculation;
use bz_build_api::interpreter::rule_defs::provider::collection::FrozenProviderCollectionValue;
use bz_build_api::query::oneshot::QUERY_FRONTEND;
use bz_cli_proto::CqueryRequest;
use bz_cli_proto::CqueryResponse;
use bz_cli_proto::HasClientContext;
use bz_common::dice::cells::HasCellResolver;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::provider::label::ProvidersName;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_node::attrs::display::AttrDisplayWithContext;
use bz_node::attrs::display::AttrDisplayWithContextExt;
use bz_node::attrs::fmt_context::AttrFmtContext;
use bz_node::attrs::serialize::AttrSerializeWithContext;
use bz_node::nodes::configured::ConfiguredTargetNode;
use bz_query::query::environment::AttrFmtOptions;
use bz_query::query::syntax::simple::eval::values::QueryEvaluationResult;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use bz_server_ctx::template::ServerCommandTemplate;
use bz_server_ctx::template::run_server_command;
use bz_util::truncate::truncate;
use dice::DiceTransaction;
use dice::LinearRecomputeDiceComputations;
use dupe::Dupe;

use crate::query::printer::ProviderLookUp;
use crate::query::printer::QueryResultPrinter;
use crate::query::printer::ShouldPrintProviders;
use crate::query::query_target_ext::QueryCommandTarget;
use crate::query::starlark_profile::write_query_profile_for_targets;

impl QueryCommandTarget for ConfiguredTargetNode {
    fn call_stack(&self) -> Option<String> {
        ConfiguredTargetNode::call_stack(self)
    }

    fn attr_to_string_alternate(&self, options: AttrFmtOptions, attr: &Self::Attr<'_>) -> String {
        format!(
            "{:#}",
            attr.as_display(&AttrFmtContext {
                package: Some(self.label().pkg().dupe()),
                options
            })
        )
    }

    fn attr_serialize<S: serde::Serializer>(
        &self,
        attr: &Self::Attr<'_>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        attr.serialize_with_ctx(
            &AttrFmtContext {
                package: Some(self.label().pkg().dupe()),
                options: Default::default(),
            },
            serializer,
        )
    }

    fn attr_fmt(
        &self,
        fmt: &mut std::fmt::Formatter<'_>,
        options: AttrFmtOptions,
        attr: &Self::Attr<'_>,
    ) -> std::fmt::Result {
        AttrDisplayWithContext::fmt(
            attr,
            &AttrFmtContext {
                package: Some(self.label().pkg().dupe()),
                options,
            },
            fmt,
        )
    }
}

pub(crate) async fn cquery_command(
    ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
    req: CqueryRequest,
) -> bz_error::Result<CqueryResponse> {
    run_server_command(CqueryServerCommand { req }, ctx, partial_result_dispatcher).await
}

struct CqueryServerCommand {
    req: CqueryRequest,
}

#[async_trait]
impl ServerCommandTemplate for CqueryServerCommand {
    type StartEvent = bz_data::CQueryCommandStart;
    type EndEvent = bz_data::CQueryCommandEnd;
    type Response = CqueryResponse;
    type PartialResult = bz_cli_proto::StdoutBytes;

    fn start_event(&self) -> bz_data::CQueryCommandStart {
        bz_data::CQueryCommandStart {
            query: truncate(&self.req.query, 50000),
            query_args: truncate(&self.req.query_args.join(","), 1000),
            target_universe: truncate(&self.req.target_universe.join(","), 1000),
        }
    }

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        mut partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        ctx: DiceTransaction,
    ) -> bz_error::Result<Self::Response> {
        cquery(
            server_ctx,
            partial_result_dispatcher.as_writer(),
            ctx,
            &self.req,
        )
        .await
    }
}

async fn cquery(
    server_ctx: &dyn ServerCommandContextTrait,
    mut stdout: impl Write,
    mut ctx: DiceTransaction,
    request: &CqueryRequest,
) -> bz_error::Result<CqueryResponse> {
    let cell_resolver = ctx.get_cell_resolver().await?;
    let output_configuration = QueryResultPrinter::from_request_options(
        &cell_resolver,
        &request.output_attributes,
        request.unstable_output_format,
        request.client_context()?.trace_id.clone(),
    )?;

    let CqueryRequest {
        query,
        query_args,
        target_universe,
        context,
        show_providers,
        target_cfg,
        ..
    } = request;

    // The request will always have a universe value, an empty one indicates the user didn't provide a universe.
    let target_universe: Option<&[String]> = if target_universe.is_empty() {
        None
    } else {
        Some(target_universe)
    };
    let client_ctx = context
        .as_ref()
        .ok_or_else(|| internal_error!("No client context"))?;

    let target_call_stacks = client_ctx.target_call_stacks;

    let global_cfg_options = global_cfg_options_from_client_context(
        target_cfg
            .as_ref()
            .ok_or_else(|| internal_error!("target_cfg must be set"))?,
        server_ctx,
        &mut ctx,
    )
    .await?;

    let profile_mode = request
        .profile_mode
        .map(|i| bz_cli_proto::ProfileMode::try_from(i).internal_error("Invalid profile mode"))
        .transpose()?;

    let (query_result, universes) = QUERY_FRONTEND
        .get()?
        .eval_cquery(
            &mut ctx,
            server_ctx.working_dir(),
            query,
            query_args,
            global_cfg_options,
            target_universe,
            profile_mode.is_some(),
        )
        .await?;

    if let Some(profile_mode) = profile_mode {
        let universes = universes.ok_or_else(|| internal_error!("No universes"))?;
        if universes.is_empty() {
            // Sanity check.
            return Err(internal_error!("Empty universes list"));
        }

        write_query_profile_for_targets(
            &mut ctx,
            profile_mode,
            request.profile_output.as_deref(),
            universes.iter().flat_map(|u| {
                u.iter()
                    .map(|t| t.label().unconfigured().pkg())
                    // `collect` should not be needed, but I was defeated by the compiler.
                    .collect::<Vec<_>>()
            }),
        )
        .await?;
    } else if universes.is_some() {
        return Err(internal_error!("We did not request universes"));
    }

    ctx.with_linear_recompute(|ctx| async move {
        let should_print_providers = if *show_providers {
            ShouldPrintProviders::Yes(&ctx as &dyn ProviderLookUp<ConfiguredTargetNode>)
        } else {
            ShouldPrintProviders::No
        };

        match query_result {
            QueryEvaluationResult::Single(targets) => {
                output_configuration
                    .print_single_output(
                        &mut stdout,
                        targets,
                        target_call_stacks,
                        should_print_providers,
                    )
                    .await?
            }
            QueryEvaluationResult::Multiple(results) => {
                output_configuration
                    .print_multi_output(
                        &mut stdout,
                        results,
                        target_call_stacks,
                        should_print_providers,
                    )
                    .await?
            }
        };
        bz_error::Ok(())
    })
    .await?;

    Ok(CqueryResponse {})
}

#[async_trait]
impl ProviderLookUp<ConfiguredTargetNode> for LinearRecomputeDiceComputations<'_> {
    async fn lookup(
        &self,
        t: &ConfiguredTargetNode,
    ) -> bz_error::Result<MaybeCompatible<FrozenProviderCollectionValue>> {
        Ok(self
            .get()
            .get_providers(&ConfiguredProvidersLabel::new(
                t.label().dupe(),
                ProvidersName::Default,
            ))
            .await?)
    }
}
