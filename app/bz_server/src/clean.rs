use bz_cli_proto::CleanRequest;
use bz_cli_proto::CleanStaleResponse;
use bz_common::file_ops::metadata::clear_computed_file_digest_cache;
use bz_core::fs::buck_out_path::BazelOutputRoot;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_error::BuckErrorContext;
use bz_error::internal_error;
use bz_events::dispatch::span_async;
use bz_execute::execute::clean_output_paths::BackgroundCleanOutputPaths;
use bz_execute::execute::clean_output_paths::CleanOutputPaths;
use bz_fs::paths::forward_rel_path::ForwardRelativePath;
use bz_server_ctx::commands::command_end;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::partial_result_dispatcher::NoPartialResult;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use dupe::Dupe;

use crate::ctx::ServerCommandContext;

pub(crate) async fn clean_command(
    context: &ServerCommandContext<'_>,
    _partial_result_dispatcher: PartialResultDispatcher<NoPartialResult>,
    req: CleanRequest,
) -> bz_error::Result<CleanStaleResponse> {
    let start_event = context
        .command_start_event(bz_data::CleanCommandStart {}.into())
        .await?;
    span_async(start_event, async {
        let result = clean_impl(context, req).await;
        let clean_stale_stats = result.as_ref().ok().and_then(|res| res.stats.clone());
        let end_event = command_end(&result, bz_data::CleanCommandEnd { clean_stale_stats });
        (result, end_event)
    })
    .await
}

async fn clean_impl(
    context: &ServerCommandContext<'_>,
    req: CleanRequest,
) -> bz_error::Result<CleanStaleResponse> {
    let output_roots = normal_clean_output_roots(context);
    if req.dry_run {
        return Ok(CleanStaleResponse {
            message: Some(format!(
                "Would clean output roots:\n{}",
                output_roots
                    .iter()
                    .map(|path| format!("  {path}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            stats: None,
        });
    }

    if !req.dry_run {
        context
            .base_context
            .daemon
            .local_action_cache
            .clear()
            .await
            .buck_error_context("Failed to clear local action cache")?;

        context
            .base_context
            .daemon
            .incremental_db_state
            .clear()
            .buck_error_context("Failed to clear incremental state")?;

        clear_computed_file_digest_cache();

        context
            .base_context
            .daemon
            .dice_manager
            .reset_dice(context.events().dupe(), "clean".to_owned())
            .await
            .buck_error_context("Failed to reset DICE graph")?;
    }

    let materializer = context.materializer();
    let extension = materializer
        .as_deferred_materializer_extension()
        .ok_or_else(|| internal_error!("Deferred materializer is not in use"))?;

    extension
        .clear_all_artifacts()
        .await
        .buck_error_context("Failed to clear materializer artifact state")?;

    clean_output_roots(context, output_roots, req.background)
        .await
        .buck_error_context("Failed to clean output roots")?;

    Ok(CleanStaleResponse {
        message: None,
        stats: None,
    })
}

fn normal_clean_output_roots(context: &ServerCommandContext<'_>) -> Vec<ProjectRelativePathBuf> {
    let mut roots = ["art", "gen", "test", "__bazel_execroot"]
        .into_iter()
        .map(|path| {
            context
                .buck_out_dir
                .join(ForwardRelativePath::unchecked_new(path))
        })
        .collect::<Vec<_>>();

    roots.push(ProjectRelativePathBuf::unchecked_new(
        BazelOutputRoot::Bin.exec_root().to_owned(),
    ));
    roots.push(ProjectRelativePathBuf::unchecked_new(
        BazelOutputRoot::Genfiles.exec_root().to_owned(),
    ));

    roots
}

async fn clean_output_roots(
    context: &ServerCommandContext<'_>,
    paths: Vec<ProjectRelativePathBuf>,
    background: bool,
) -> bz_error::Result<()> {
    let cleaner: Box<dyn bz_execute::execute::blocking::IoRequest> = if background {
        Box::new(BackgroundCleanOutputPaths { paths })
    } else {
        Box::new(CleanOutputPaths { paths })
    };

    context
        .base_context
        .daemon
        .blocking_executor
        .execute_io(cleaner, context.cancellation_context())
        .await
}
