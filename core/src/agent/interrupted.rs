//! Telling the model which turns were cut off.
//!
//! Nothing here resumes anything. The turn that died is gone; what survives is
//! the record of where it stopped, and the model is the only thing in a
//! position to decide what that means for the work — whether the file it was
//! writing needs checking, whether the command it ran needs to be looked up
//! rather than repeated.
//!
//! Delivered on the user's next message rather than on startup, so nothing the
//! user did not ask for reaches a provider.
//!
//! Which makes delivery a thing that can fail, and therefore a thing that has
//! to be written down. Reading the record is not telling anyone: a turn can
//! read it and then die on a missing key without sending a byte. Nor is sending
//! it — a provider can answer 200 and refuse over SSE, having processed
//! nothing. Only a reply read to the end settles anything, and until one is,
//! every turn still owed an explanation keeps being owed it.

use diesel::sqlite::SqliteConnection;

use crate::db::models::turn::{TurnPhase, TurnRow, TurnStatus};
use crate::db::ops::turn::{InterruptedCandidate, Ledger};
use crate::turn::{TurnCoordinator, TurnOrigin};

/// Whether a recorded turn is one that stopped without finishing.
///
/// `status` alone cannot answer this. A turn writes `running` when it starts
/// and overwrites it when it reaches an ending, so anything that never reached
/// one — a kill, a panic, a task dropped at shutdown — leaves the row saying
/// `running` forever. Startup reconciliation rewrites those, but only at
/// startup: a turn that panicked a minute ago still says `running` and would
/// otherwise read as a turn still in progress.
///
/// The coordinator settles it. It is the live register of what is actually
/// running, so a `running` row it does not hold is a turn that is not running,
/// whatever the row says. Startup reconciliation is then just the degenerate
/// case of the same rule — an empty coordinator — persisted so the conclusion
/// does not have to be re-derived forever.
///
/// `held` is the turn the coordinator has on this conversation, read once for
/// however many rows are being judged. Asking it per row would let a list be
/// answered against two different moments.
pub fn was_cut_off(turn: &TurnRow, held: Option<&str>) -> Result<bool, String> {
    Ok(match turn.status()? {
        TurnStatus::Interrupted => true,
        TurnStatus::Running => held != Some(turn.id.as_str()),
        TurnStatus::WaitingReview | TurnStatus::Done | TurnStatus::Cancelled | TurnStatus::Failed => false,
    })
}

/// How many cut-off turns one message may describe.
///
/// Reached only by crashing repeatedly without a single request getting out in
/// between, so it bounds a pathology rather than normal use. Nothing beyond it
/// is discarded: an unreported turn stays unreported and comes back on the next
/// message. What the cap must not do is let a newer interruption bury an older,
/// more dangerous one — see `choose`.
const AT_MOST: usize = 3;

/// How far back a single read looks for turns still owed an explanation.
///
/// Only a hard stop on the query. Beyond it the same rule applies as beyond
/// `AT_MOST`: still owed, told later.
const WINDOW: i64 = 20;

/// What the model is told, and which turns it settles.
///
/// The two travel together because they must be decided together: telling the
/// model is what makes a turn reported, and a turn is only reported if the
/// telling actually left the machine.
pub struct Report {
    text: String,
    /// Which turn, and in which ledger. A delegated run is owed to two
    /// conversations and settled separately in each, so the id alone would not
    /// say what to write down.
    turns: Vec<(String, Ledger)>,
}

impl Report {
    pub fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    fn turn_ids(&self) -> Vec<&str> {
        self.turns.iter().map(|(id, _)| id.as_str()).collect()
    }
}

/// What to tell the model about turns that stopped without finishing.
///
/// `asking` is the turn being assembled, which by now has a record of its own
/// and must not be mistaken for one of the ones that came before it.
///
/// Reading this settles nothing. The turn that reads it may itself die before
/// reaching a provider — on a missing key, an unreadable config, a context that
/// will not build — and then nobody has been told anything. Only
/// `confirm_delivered`, called once a reply has been read to the end, writes
/// that down.
pub(crate) fn block(
    conn: &mut SqliteConnection,
    coordinator: &TurnCoordinator,
    conversation_id: &str,
    asking: Option<&str>,
) -> Result<Option<Report>, String> {
    let candidates = crate::db::ops::turn::unreported_for_conversation(conn, conversation_id, asking, WINDOW)
        .map_err(|error| error.to_string())?;
    // Per conversation, because a delegated run is held on its own. Judging a
    // sub-agent against the parent's lease would call every live one a wreck.
    // Read once per conversation rather than once per row, so one list is never
    // answered against two different moments.
    let mut held: std::collections::HashMap<String, Option<String>> = Default::default();
    for c in &candidates {
        held.entry(c.turn.conversation_id.clone())
            .or_insert_with(|| coordinator.held_turn(&c.turn.conversation_id));
    }
    // Newest first, the order the query returns.
    let mut cut_off = Vec::new();
    for candidate in &candidates {
        let held_turn = held
            .get(&candidate.turn.conversation_id)
            .and_then(|turn| turn.as_deref());
        if was_cut_off(&candidate.turn, held_turn)? {
            cut_off.push(candidate);
        }
    }
    let picked = choose(&cut_off)?;
    if picked.is_empty() {
        return Ok(None);
    }
    Ok(Some(Report {
        text: describe(&picked)?,
        turns: picked.iter().map(|c| (c.turn.id.clone(), c.ledger)).collect(),
    }))
}

/// Which of the turns still owed an explanation this message carries, oldest
/// first.
///
/// `cut_off` arrives newest first. Recency is the obvious way to cap it and the
/// wrong one on its own: the turn that matters is the one that may have left a
/// file half-written, and that is a property of what it was doing, not of when
/// it happened. A newer interruption is not evidence about an older one and
/// must not be allowed to push it out of the message — so anything caught
/// inside a tool is taken first, and only then is the rest of the room filled
/// by recency.
///
/// Whatever does not fit is not dropped. It is still unreported, so the next
/// message asks the same question and gets it.
fn choose<'a>(cut_off: &[&'a InterruptedCandidate]) -> Result<Vec<&'a InterruptedCandidate>, String> {
    let mut chosen = Vec::new();
    for (index, candidate) in cut_off.iter().enumerate() {
        if candidate.turn.phase()? == Some(TurnPhase::RunningTool) {
            chosen.push(index);
            if chosen.len() == AT_MOST {
                break;
            }
        }
    }
    for i in 0..cut_off.len() {
        if chosen.len() >= AT_MOST {
            break;
        }
        if !chosen.contains(&i) {
            chosen.push(i);
        }
    }
    // Indices count backwards through time, so descending is the order the
    // interruptions actually happened in.
    chosen.sort_unstable_by(|a, b| b.cmp(a));
    Ok(chosen.into_iter().map(|i| cut_off[i]).collect())
}

/// Async wrapper for the runners, which hold a pool rather than a connection.
pub async fn load_block(
    pool: &crate::db::DbPool,
    coordinator: &std::sync::Arc<TurnCoordinator>,
    conversation_id: &str,
    asking: &str,
) -> Result<Option<Report>, String> {
    let pool = pool.clone();
    let coordinator = std::sync::Arc::clone(coordinator);
    let conv = conversation_id.to_string();
    let asking = asking.to_string();
    tokio::task::spawn_blocking(move || {
        let mut conn = crate::util::get_conn(&pool)?;
        block(&mut conn, &coordinator, &conv, Some(&asking))
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Record that these turns have now been described to the model.
///
/// Called at the one moment that proves it: a reply read to the end. Not when
/// the block is read — a turn can read it and die on the way — nor when the
/// stream opens, because a provider that answers 200 and then refuses over SSE
/// processed none of it. And not when the turn ends either, because by then it
/// may have ended badly for reasons that have nothing to do with whether the
/// model saw this.
///
/// Every way of getting it wrong is therefore a repeated warning rather than a
/// lost one, including a write that fails. That is the direction to fail in:
/// saying twice that a tool may be half-run costs a paragraph, saying it zero
/// times costs whatever the model does next.
pub(crate) async fn confirm_delivered(pool: &crate::db::DbPool, report: Report) {
    let pool = pool.clone();
    let turns = report.turns;
    let written = tokio::task::spawn_blocking(move || {
        let mut conn = pool.get().map_err(|e| e.to_string())?;
        let now = crate::util::now_ms();
        // Each audience settles only its own ledger. A parent being told is not
        // the sub-agent's conversation being told, and the other way round.
        let mut written = 0;
        for ledger in [Ledger::Own, Ledger::Parent] {
            let ids: Vec<String> = turns
                .iter()
                .filter(|(_, l)| *l == ledger)
                .map(|(id, _)| id.clone())
                .collect();
            written += crate::db::ops::turn::mark_reported(&mut conn, &ids, ledger, now).map_err(|e| e.to_string())?;
        }
        Ok::<usize, String>(written)
    })
    .await;
    match written {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not record that an interruption was reported"),
        Err(e) => tracing::warn!(error = %e, "recording a reported interruption panicked"),
    }
}

fn describe(turns: &[&InterruptedCandidate]) -> Result<String, String> {
    let each: Vec<String> = turns
        .iter()
        .map(|candidate| what_happened(candidate))
        .collect::<Result<_, _>>()?;
    // Says that it stopped, not why. The rule that gets a turn here — running,
    // and nobody holding it — is met by a process that was killed, by a task
    // that panicked, and by one dropped at shutdown, and the record cannot tell
    // them apart. Naming a cause the record does not have would be a guess
    // dressed as context, and the model has no way to check it.
    //
    // What is always true is that it never reached an ending. A turn that
    // failed, or that the loop guard stopped, did reach one — and said so in
    // the transcript the model can already see.
    // Says "a turn" rather than "a turn in this conversation": one of these may
    // have run in a sub-agent's transcript, and each line names its own subject.
    let body = match each.as_slice() {
        [only] => format!("A turn was cut off before it finished, and nothing recorded why. {only}"),
        many => format!(
            "Several turns were cut off before they finished, and nothing recorded why. \
             Oldest first.\n{}",
            many.iter().map(|w| format!("- {w}")).collect::<Vec<_>>().join("\n")
        ),
    };
    Ok(format!("<interrupted_turn>\n{body}\n</interrupted_turn>"))
}

fn what_happened(candidate: &InterruptedCandidate) -> Result<String, String> {
    let turn = &candidate.turn;
    let tool = turn.phase_tool.as_deref().unwrap_or("a tool");
    // Who this is about. A delegated run has to name itself: the parent's own
    // history contains a single `run_agent` call and nothing about what the
    // sub-agent was doing when it stopped, so "it" would read as the parent.
    let (who, whose) = match subject(candidate) {
        Some(s) => (s, "its own"),
        None => ("It".to_string(), "this conversation's"),
    };
    Ok(match turn.phase()? {
        // The dangerous one: the call had started, so whatever it does may
        // already be done. Saying "it failed" would be as wrong as saying it
        // succeeded, and either would have the model act on a guess.
        Some(TurnPhase::RunningTool) => format!(
            "{who} had started running {tool} and never recorded the result, so that call may have \
             taken effect, may have half-taken effect, or may not have run at all. Do not assume \
             either way — check the current state before doing anything that depends on it, and \
             do not simply repeat the call if repeating it would not be safe."
        ),
        // The safe one, and worth saying so plainly.
        Some(TurnPhase::AwaitingApproval) => format!(
            "{who} was waiting for the user to approve {tool} when it stopped. That call did not \
             run. Ask again if it is still what you need."
        ),
        Some(TurnPhase::Compacting) => format!(
            "{who} was summarising {whose} history when it stopped, so the history you can see may \
             be missing a summary it was about to write. Nothing was lost; there may just be more \
             of it than usual."
        ),
        Some(TurnPhase::Streaming) | None => format!(
            "{who} stopped part way through writing a reply. Anything it had begun to say is \
             incomplete."
        ),
    })
}

/// How a delegated run introduces itself, or `None` for a turn of this
/// conversation's own.
///
/// Keyed off `origin` rather than off which ledger it came back in: the column
/// is on the row, and it stays right if a candidate is ever reached another way.
fn subject(candidate: &InterruptedCandidate) -> Option<String> {
    if TurnOrigin::parse(&candidate.turn.origin) != Ok(TurnOrigin::SubAgent) {
        return None;
    }
    // The title is the description the parent wrote when it delegated, so it is
    // the one phrase that identifies the errand in the parent's own words.
    Some(
        match candidate
            .child_title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(title) => format!("A sub-agent you delegated to (\"{title}\")"),
            None => "A sub-agent you delegated to".to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::turn::ERROR_LOOP_DETECTED;
    use crate::db::ops::conversation::create_conversation;
    use crate::db::ops::turn;
    use crate::db::test_db;
    use crate::turn::TurnOrigin;
    use std::sync::Arc;

    fn setup() -> (crate::db::DbPool, Arc<TurnCoordinator>) {
        let pool = test_db();
        {
            let mut conn = pool.get().unwrap();
            create_conversation(&mut conn, "c1", Some("t"), None, None, 1).unwrap();
        }
        (pool, Arc::new(TurnCoordinator::new()))
    }

    fn latest(pool: &crate::db::DbPool, coordinator: &TurnCoordinator) -> Option<Report> {
        let mut conn = pool.get().unwrap();
        block(&mut conn, coordinator, "c1", None).unwrap()
    }

    /// What the real caller looks like: a turn that has already opened its own
    /// record asking about the ones before it. Without the exclusion it would
    /// find itself — running, and held — and report nothing.
    fn asked_by(pool: &crate::db::DbPool, coordinator: &TurnCoordinator, asking: &str) -> Option<Report> {
        let mut conn = pool.get().unwrap();
        block(&mut conn, coordinator, "c1", Some(asking)).unwrap()
    }

    /// The other half of a real caller: the request got out, so what it carried
    /// is now on record as told.
    fn delivered(pool: &crate::db::DbPool, report: Report, at: i64) {
        let mut conn = pool.get().unwrap();
        for ledger in [Ledger::Own, Ledger::Parent] {
            let ids: Vec<String> = report
                .turns
                .iter()
                .filter(|(_, l)| *l == ledger)
                .map(|(id, _)| id.clone())
                .collect();
            turn::mark_reported(&mut conn, &ids, ledger, at).unwrap();
        }
    }

    #[test]
    fn a_conversation_with_no_turns_says_nothing() {
        let (pool, c) = setup();
        assert!(latest(&pool, &c).is_none());
    }

    #[test]
    fn a_turn_that_finished_says_nothing() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::finish(&mut conn, "t1", TurnStatus::Done, None, 1500).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none());
    }

    /// Reconciliation only runs at startup, so a turn that died in this process
    /// still says `running`. Trusting the column would miss it entirely.
    #[test]
    fn a_turn_still_marked_running_but_held_by_nobody_was_cut_off() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::set_phase(&mut conn, "t1", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        drop(conn);

        let told = latest(&pool, &c).expect("a turn nobody is running was cut off");
        assert!(told.text().contains("edit_file"));
        assert!(told.text().contains("may have taken effect"));
    }

    /// The other half of the same rule: a turn the coordinator really is
    /// running is not an interruption, and saying so would be a lie told to the
    /// model about work still in progress.
    #[test]
    fn a_turn_that_is_actually_running_is_not_an_interruption() {
        let (pool, c) = setup();
        let lease = c.try_acquire_turn_as("c1", TurnOrigin::Desktop, "t1".into()).unwrap();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none());

        // And once it goes away without finishing, it is.
        drop(lease);
        assert!(latest(&pool, &c).is_some());
    }

    /// The coordinator is keyed by conversation, so holding *a* turn is not
    /// holding *this* turn — a new turn running on the same conversation must
    /// not make the dead one before it look alive.
    #[test]
    fn a_newer_turn_does_not_vouch_for_an_older_one() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "dead", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        drop(conn);
        let _live = c.try_acquire_turn_as("c1", TurnOrigin::Desktop, "live".into()).unwrap();

        // `dead` is still the most recent recorded turn, and it is not the one
        // being held.
        assert!(latest(&pool, &c).is_some());
    }

    /// The shape every real call has: the asking turn has already recorded
    /// itself, so it is the newest unfinished row on the conversation and would
    /// otherwise be an answer to its own question.
    ///
    /// Two independent things keep it out — the exclusion, and the coordinator
    /// holding it — and the test pins both, because each covers a case the
    /// other does not. The exclusion holds even for a turn the coordinator has
    /// already let go of; the coordinator holds even for a caller that passes
    /// no exclusion at all, which the token estimator does.
    #[test]
    fn a_turn_asking_about_the_one_before_it_does_not_find_itself() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "dead", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::set_phase(&mut conn, "dead", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        drop(conn);

        // The new turn takes the conversation and opens its record, exactly as
        // the runner does before assembling its request.
        let _live = c.try_acquire_turn_as("c1", TurnOrigin::Desktop, "live".into()).unwrap();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "live", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        drop(conn);

        let told = asked_by(&pool, &c, "live").expect("the turn before this one was cut off");
        assert_eq!(told.turn_ids(), ["dead"]);
        assert!(told.text().contains("edit_file"));

        // Without the exclusion the asking turn is a candidate row, and only
        // the coordinator keeps it out.
        let unfiltered = latest(&pool, &c).expect("the dead turn is still reported");
        assert_eq!(
            unfiltered.turn_ids(),
            ["dead"],
            "a turn being run is not a turn that was cut off"
        );
    }

    #[test]
    fn the_startup_verdict_is_believed_without_asking_again() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::set_phase(&mut conn, "t1", TurnPhase::AwaitingApproval, Some("run_command"), 1001).unwrap();
        turn::reconcile_interrupted(&mut conn, 2000).unwrap();
        drop(conn);

        let told = latest(&pool, &c).expect("a reconciled turn is still an interrupted one");
        assert!(told.text().contains("run_command"));
        assert!(
            told.text().contains("did not run"),
            "an approval that was never given ran nothing",
        );
    }

    #[test]
    fn each_phase_says_something_different() {
        let (pool, c) = setup();
        for (phase, tool, expect) in [
            (TurnPhase::Streaming, None, "part way through writing a reply"),
            (TurnPhase::Compacting, None, "summarising this conversation"),
            (TurnPhase::AwaitingApproval, Some("run_command"), "did not run"),
            (TurnPhase::RunningTool, Some("edit_file"), "may have taken effect"),
        ] {
            let mut conn = pool.get().unwrap();
            let id = format!("t-{}", phase.as_str());
            turn::begin(&mut conn, &id, "c1", TurnOrigin::Desktop, None, 1000).unwrap();
            turn::set_phase(&mut conn, &id, phase, tool, 1001).unwrap();
            drop(conn);

            let told = latest(&pool, &c).expect("cut off");
            assert!(told.text().contains(expect), "{}: {}", phase.as_str(), told.text());
            // Settle it, so the next phase is read on its own rather than
            // alongside everything before it.
            delivered(&pool, told, 1002);
        }
    }

    /// A turn that failed, or that the loop guard stopped, reached an ending —
    /// and said so where the model can already see it: the error bubble, or the
    /// loop guard's own message sitting in the transcript as a tool result.
    /// This block is only for turns that never got to say anything.
    #[test]
    fn a_turn_that_ended_badly_is_not_an_interruption() {
        let (pool, c) = setup();
        for (id, error) in [
            ("looped", Some(ERROR_LOOP_DETECTED)),
            ("broke", Some("API Key not set")),
        ] {
            let mut conn = pool.get().unwrap();
            turn::begin(&mut conn, id, "c1", TurnOrigin::Desktop, None, 1000).unwrap();
            turn::finish(&mut conn, id, TurnStatus::Failed, error, 1500).unwrap();
            drop(conn);

            assert!(latest(&pool, &c).is_none(), "{id} ended, badly but definitely");
        }
    }

    /// Stopping is a decision the user made and watched happen.
    #[test]
    fn a_turn_the_user_stopped_is_not_an_interruption() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "t1", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::finish(&mut conn, "t1", TurnStatus::Cancelled, None, 1500).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none());
    }

    /// A turn that took the notice to a provider settles it, and it does not
    /// come back.
    #[test]
    fn a_turn_that_carried_the_notice_clears_it() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "dead", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        drop(conn);

        // The good turn opens its record, reads the notice, and gets its
        // request away.
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "good", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        drop(conn);
        let told = asked_by(&pool, &c, "good").expect("the turn before it was cut off");
        assert!(told.text().contains("cut off"));
        delivered(&pool, told, 2100);

        let mut conn = pool.get().unwrap();
        turn::finish(&mut conn, "good", TurnStatus::Done, None, 2500).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none());
    }

    /// The hole the "just look at the most recent turn" version had.
    ///
    /// A turn that dies before reaching a provider carries nothing, so it
    /// cannot have settled anything — but it *is* the most recent turn, and
    /// under the old rule that alone was enough to make the warning stop. The
    /// one warning that says a tool may have half-run then disappeared without
    /// the model ever having seen it.
    #[test]
    fn a_turn_that_died_before_reaching_the_provider_settles_nothing() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "dead", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::set_phase(&mut conn, "dead", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        drop(conn);

        // The next turn reads the warning and then falls over on its way out —
        // a missing key, an unreadable config. It never sent anything.
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "stillborn", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        drop(conn);
        assert!(
            asked_by(&pool, &c, "stillborn").is_some(),
            "it read the warning, which is not the same as delivering it",
        );
        let mut conn = pool.get().unwrap();
        turn::finish(
            &mut conn,
            "stillborn",
            TurnStatus::Failed,
            Some("API Key not set"),
            2100,
        )
        .unwrap();
        drop(conn);

        // The turn after it still has to be told.
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "next", "c1", TurnOrigin::Desktop, None, 3000).unwrap();
        drop(conn);
        let told = asked_by(&pool, &c, "next").expect("nobody has told the model yet");
        assert!(told.text().contains("edit_file"));
        assert!(told.text().contains("may have taken effect"));
        assert_eq!(
            told.turn_ids(),
            ["dead"],
            "the failed turn is an ending, not an interruption"
        );
    }

    /// Crashing twice without a request getting out in between owes two
    /// explanations, and the dangerous one is the older of them — so it cannot
    /// be left for a later message.
    #[test]
    fn every_turn_still_owed_an_explanation_gets_one() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        turn::begin(&mut conn, "first", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
        turn::set_phase(&mut conn, "first", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        turn::begin(&mut conn, "second", "c1", TurnOrigin::Desktop, None, 2000).unwrap();
        turn::set_phase(&mut conn, "second", TurnPhase::Compacting, None, 2001).unwrap();
        drop(conn);

        let told = latest(&pool, &c).expect("both are still owed");
        assert_eq!(told.turn_ids(), ["first", "second"], "oldest first, as they happened");
        let at = |needle: &str| told.text().find(needle).unwrap_or(usize::MAX);
        assert!(at("edit_file") < at("summarising"), "{}", told.text());
        assert!(told.text().contains("Several turns"));

        delivered(&pool, told, 3000);
        assert!(latest(&pool, &c).is_none());
    }

    /// Nothing beyond the cap is thrown away — it is only deferred, and the
    /// newest are the ones that go first because they are the ones with a
    /// bearing on what happens next.
    #[test]
    fn more_wrecks_than_fit_are_deferred_rather_than_dropped() {
        let (pool, c) = setup();
        {
            let mut conn = pool.get().unwrap();
            for n in 0..(AT_MOST as i64 + 1) {
                turn::begin(&mut conn, &format!("t{n}"), "c1", TurnOrigin::Desktop, None, 1000 + n).unwrap();
            }
        }

        let told = latest(&pool, &c).expect("four wrecks");
        assert_eq!(told.turn_ids(), ["t1", "t2", "t3"]);
        delivered(&pool, told, 5000);

        let rest = latest(&pool, &c).expect("the oldest one is still owed");
        assert_eq!(rest.turn_ids(), ["t0"]);
        delivered(&pool, rest, 5001);
        assert!(latest(&pool, &c).is_none());
    }

    /// The cap must not let recency decide what matters. Three later
    /// interruptions say nothing at all about a tool that was mid-execution
    /// before them, so they cannot be the reason the model is not told about it.
    #[test]
    fn a_newer_wreck_cannot_bury_an_older_one_that_was_inside_a_tool() {
        let (pool, c) = setup();
        {
            let mut conn = pool.get().unwrap();
            turn::begin(&mut conn, "wrote-a-file", "c1", TurnOrigin::Desktop, None, 1000).unwrap();
            turn::set_phase(
                &mut conn,
                "wrote-a-file",
                TurnPhase::RunningTool,
                Some("edit_file"),
                1001,
            )
            .unwrap();
            for n in 0..(AT_MOST as i64) {
                turn::begin(
                    &mut conn,
                    &format!("later{n}"),
                    "c1",
                    TurnOrigin::Desktop,
                    None,
                    2000 + n,
                )
                .unwrap();
            }
        }

        let told = latest(&pool, &c).expect("four wrecks, one of them dangerous");
        assert!(
            told.turn_ids().contains(&"wrote-a-file"),
            "the tool that may have taken effect went first: {:?}",
            told.turn_ids(),
        );
        assert_eq!(told.turn_ids()[0], "wrote-a-file", "and it is still told oldest first");
        assert!(told.text().contains("edit_file"));
        assert_eq!(told.turn_ids().len(), AT_MOST);
    }

    /// Turns belong to their conversation; one that died elsewhere is not this
    /// conversation's business — including a delegated run somebody *else*
    /// started, which reaches its own parent and no further.
    #[test]
    fn another_conversations_wreck_is_not_reported_here() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        create_conversation(&mut conn, "c2", Some("t"), None, None, 1).unwrap();
        turn::begin(&mut conn, "t1", "c2", TurnOrigin::Desktop, None, 1000).unwrap();
        // And a sub-agent belonging to that other conversation.
        delegated(&mut conn, "elsewhere", "c2", Some("their errand"));
        turn::begin(&mut conn, "theirs", "elsewhere", TurnOrigin::SubAgent, None, 1000).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none());
    }

    /// A sub-agent's conversation, as `DesktopSubAgents` opens one.
    fn delegated(conn: &mut SqliteConnection, id: &str, parent: &str, title: Option<&str>) {
        crate::db::ops::conversation::insert(
            conn,
            crate::db::models::conversation::ConversationInsert {
                id,
                title,
                parent_conversation_id: Some(parent),
                created_at: 1,
                updated_at: 1,
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// The reason this batch exists. What the parent's own record can say is
    /// "`run_agent` may or may not have run"; the fact that matters — a file was
    /// being written — is on a row in a conversation the parent has never
    /// mentioned.
    #[test]
    fn a_sub_agent_caught_inside_a_tool_is_reported_to_the_parent() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", Some("check the failing test"));
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        turn::set_phase(&mut conn, "run", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        drop(conn);

        let told = latest(&pool, &c).expect("the parent is owed this");
        assert_eq!(told.turn_ids(), ["run"]);
        assert!(told.text().contains("A sub-agent you delegated to"), "{}", told.text());
        assert!(told.text().contains("check the failing test"), "{}", told.text());
        assert!(told.text().contains("edit_file"));
        assert!(told.text().contains("may have taken effect"));
    }

    /// A title is the description the parent wrote, and nothing guarantees there
    /// was one. Missing, it says less rather than showing empty quotes.
    #[test]
    fn a_sub_agent_with_no_title_still_introduces_itself() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", None);
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        drop(conn);

        let told = latest(&pool, &c).expect("cut off");
        assert!(told.text().contains("A sub-agent you delegated to"), "{}", told.text());
        assert!(!told.text().contains("(\"\")"), "{}", told.text());
        assert!(!told.text().contains("()"), "{}", told.text());
    }

    /// Two audiences, two ledgers. With one column, the user opening the
    /// sub-agent and typing a single message would consume the parent's notice —
    /// and the parent would go on working as if nothing had been left half-done.
    #[test]
    fn the_sub_agents_own_conversation_being_told_does_not_settle_the_parents_debt() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", Some("an errand"));
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        turn::set_phase(&mut conn, "run", TurnPhase::RunningTool, Some("edit_file"), 1001).unwrap();
        drop(conn);

        // The user opens the sub-agent and asks it something. That turn carries
        // the notice, so the sub-agent's own conversation is settled.
        let inside = {
            let mut conn = pool.get().unwrap();
            block(&mut conn, &c, "sub-1", None)
                .unwrap()
                .expect("its own history was cut off")
        };
        assert_eq!(inside.turn_ids(), ["run"]);
        delivered(&pool, inside, 2000);

        let told = latest(&pool, &c).expect("the parent has still not been told");
        assert_eq!(told.turn_ids(), ["run"]);
        assert!(told.text().contains("edit_file"));

        // And once the parent has been told, it stops asking — without having
        // un-told the sub-agent.
        delivered(&pool, told, 3000);
        assert!(latest(&pool, &c).is_none());
        let mut conn = pool.get().unwrap();
        assert!(block(&mut conn, &c, "sub-1", None).unwrap().is_none());
    }

    /// The same rule from the other side: the parent hearing about it is not
    /// the sub-agent's own transcript hearing about it.
    #[test]
    fn the_parent_being_told_does_not_settle_the_sub_agents_own_debt() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", Some("an errand"));
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        drop(conn);

        delivered(&pool, latest(&pool, &c).expect("the parent is owed it"), 2000);

        let mut conn = pool.get().unwrap();
        let inside = block(&mut conn, &c, "sub-1", None)
            .unwrap()
            .expect("the conversation it ran in has not been told");
        assert_eq!(inside.turn_ids(), ["run"]);
    }

    /// What the user typed into the sub-agent afterwards is between them and
    /// that conversation. The parent never saw the question, so an interruption
    /// there is not something it can be asked to reason about.
    #[test]
    fn a_follow_up_chat_inside_a_sub_agent_is_not_the_parents_business() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", Some("an errand"));
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        turn::finish(&mut conn, "run", TurnStatus::Done, None, 1500).unwrap();
        // The user follows up inside the sub-agent, and that turn is cut off.
        turn::begin(&mut conn, "follow-up", "sub-1", TurnOrigin::Desktop, None, 2000).unwrap();
        drop(conn);

        assert!(latest(&pool, &c).is_none(), "the parent has no business with it");

        let mut conn = pool.get().unwrap();
        let inside = block(&mut conn, &c, "sub-1", None)
            .unwrap()
            .expect("but that conversation does");
        assert_eq!(inside.turn_ids(), ["follow-up"]);
    }

    /// Liveness is per conversation. A sub-agent runs under its own lease, so
    /// judging it against the parent's would report every running one as a
    /// wreck — while it is still working.
    #[test]
    fn a_sub_agent_that_is_actually_running_is_not_an_interruption() {
        let (pool, c) = setup();
        let mut conn = pool.get().unwrap();
        delegated(&mut conn, "sub-1", "c1", Some("an errand"));
        turn::begin(&mut conn, "run", "sub-1", TurnOrigin::SubAgent, None, 1000).unwrap();
        drop(conn);
        let lease = c
            .try_acquire_turn_as("sub-1", TurnOrigin::SubAgent, "run".into())
            .unwrap();

        assert!(latest(&pool, &c).is_none(), "it is still working");

        drop(lease);
        assert!(latest(&pool, &c).is_some(), "and once nobody holds it, it was cut off");
    }
}
