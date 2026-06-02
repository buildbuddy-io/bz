use std::future::Future;

pub(crate) async fn dice_state_update_stage<T, Fut>(
    stage: impl Into<String>,
    fut: Fut,
) -> bz_error::Result<T>
where
    Fut: Future<Output = bz_error::Result<T>>,
{
    bz_events::dispatch::span_async(
        bz_data::DiceStateUpdateStageStart {
            stage: stage.into(),
        },
        async { (fut.await, bz_data::DiceStateUpdateStageEnd {}) },
    )
    .await
}
