/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bz_cli_proto::ProfileRequest;
use bz_cli_proto::ProfileResponse;
use bz_cli_proto::profile_request::ProfileOpts;
use bz_common::dice::cells::HasCellResolver;
use bz_error::bz_error;
use bz_error::internal_error;
use bz_fs::paths::abs_path::AbsPath;
use bz_interpreter::starlark_profiler::config::GetStarlarkProfilerInstrumentation;
use bz_interpreter::starlark_profiler::mode::StarlarkProfileMode;
use bz_profile::get_profile_response;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::global_cfg_options::global_cfg_options_from_client_context;
use bz_server_ctx::partial_result_dispatcher::NoPartialResult;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use bz_server_ctx::template::ServerCommandTemplate;
use bz_server_ctx::template::run_server_command;
use dice::DiceTransaction;
use futures::FutureExt;

use crate::bxl::eval::BxlResolvedCliArgs;
use crate::bxl::eval::eval;
use crate::bxl::key::BxlKey;
use crate::command::get_bxl_cli_args;
use crate::command::parse_bxl_label_from_cli;

pub(crate) async fn bxl_profile_command(
    ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    req: ProfileRequest,
) -> bz_error::Result<ProfileResponse> {
    run_server_command(
        BxlProfileServerCommand { req },
        ctx,
        partial_result_dispatcher,
    )
    .await
}

struct BxlProfileServerCommand {
    req: ProfileRequest,
}

#[async_trait]
impl ServerCommandTemplate for BxlProfileServerCommand {
    type StartEvent = bz_data::ProfileCommandStart;
    type EndEvent = bz_data::ProfileCommandEnd;
    type Response = bz_cli_proto::ProfileResponse;
    type PartialResult = NoPartialResult;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        _partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        mut ctx: DiceTransaction,
    ) -> bz_error::Result<Self::Response> {
        let ProfileOpts::BxlProfile(opts) = self
            .req
            .profile_opts
            .as_ref()
            .expect("BXL profile opts not populated")
        else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Expected BXL profile opts, not target profile opts"
            ));
        };

        let output = AbsPath::new(Path::new(&self.req.destination_path))?;

        let cwd = server_ctx.working_dir();

        let cell_resolver = ctx.get_cell_resolver().await?;
        let cell_alias_resolver = ctx.get_cell_alias_resolver_for_dir(cwd).await?;
        let bxl_label =
            parse_bxl_label_from_cli(cwd, &opts.bxl_label, &cell_resolver, &cell_alias_resolver)?;

        let global_cfg_options = global_cfg_options_from_client_context(
            opts.target_cfg
                .as_ref()
                .ok_or_else(|| internal_error!("target_cfg must be set"))?,
            server_ctx,
            &mut ctx,
        )
        .await?;

        let BxlResolvedCliArgs::Resolved(bxl_args) = get_bxl_cli_args(
            cwd,
            &mut ctx,
            &bxl_label,
            &opts.bxl_args,
            &cell_resolver,
            &global_cfg_options,
        )
        .await?
        else {
            return Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Help docs were displayed. No profiler data available"
            ));
        };
        let bxl_args = Arc::new(bxl_args);

        let bxl_key = BxlKey::new(
            bxl_label.clone(),
            bxl_args,
            /* force print stacktrace */ false,
            global_cfg_options,
        );

        if let StarlarkProfileMode::None = ctx
            .get_starlark_profiler_mode(&bxl_key.as_starlark_eval_kind())
            .await?
        {
            return Err(internal_error!("Incorrect profile mode"));
        }

        let profile_data = server_ctx
            .cancellation_context()
            .with_structured_cancellation(|observer| {
                async move {
                    bz_error::Ok(
                        eval(&mut ctx, bxl_key, observer)
                            .await
                            .map_err(|e| e.error)?
                            .1
                            .expect("No bxl profile data found"),
                    )
                }
                .boxed()
            })
            .await?;

        Ok(get_profile_response(
            profile_data,
            &[bxl_label.to_string()],
            output,
        )?)
    }
}
