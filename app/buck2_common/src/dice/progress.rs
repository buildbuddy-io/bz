use std::future::Future;

pub(crate) async fn dice_state_update_stage<T, Fut>(
    stage: impl Into<String>,
    fut: Fut,
) -> buck2_error::Result<T>
where
    Fut: Future<Output = buck2_error::Result<T>>,
{
    buck2_events::dispatch::span_async(
        buck2_data::DiceStateUpdateStageStart {
            stage: stage.into(),
        },
        async { (fut.await, buck2_data::DiceStateUpdateStageEnd {}) },
    )
    .await
}
