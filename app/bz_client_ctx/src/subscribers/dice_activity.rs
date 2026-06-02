use bz_event_observer::dice_state::DiceState;

pub(crate) fn active_dice_summary(dice_state: &DiceState) -> Option<String> {
    let mut computing = DiceActivity::default();
    let mut checking_deps = DiceActivity::default();
    let mut pending = DiceActivity::default();

    for (key, state) in dice_state.key_states() {
        computing.add(
            key,
            u64::from(state.compute_started.saturating_sub(state.compute_finished)),
        );
        checking_deps.add(
            key,
            u64::from(
                state
                    .check_deps_started
                    .saturating_sub(state.check_deps_finished),
            ),
        );
        pending.add(key, u64::from(state.started.saturating_sub(state.finished)));
    }

    if let Some(summary) = computing.summary("computing") {
        Some(summary)
    } else if let Some(summary) = checking_deps.summary("checking deps for") {
        Some(summary)
    } else {
        pending.summary("waiting on")
    }
}

#[derive(Default)]
struct DiceActivity<'a> {
    total: u64,
    top_key: Option<&'a str>,
    top_count: u64,
}

impl<'a> DiceActivity<'a> {
    fn add(&mut self, key: &'a str, count: u64) {
        if count == 0 {
            return;
        }
        self.total += count;
        if count > self.top_count {
            self.top_key = Some(key);
            self.top_count = count;
        }
    }

    fn summary(&self, verb: &str) -> Option<String> {
        let top_key = self.top_key?;
        Some(format!(
            "Buck2 graph work -- {verb} {top_key} ({} of {} active DICE keys)",
            self.top_count, self.total
        ))
    }
}
