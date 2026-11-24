//! Backend-neutral checks of the coordinator contract (design §2.2) over
//! the event log a test collected. They use `hub-coord` types only, so
//! that they can move into `hub_coord::testkit` (plan PR J) and run
//! against every backend.

use hub_coord::{CoordEvent, Progress, RequestId, SessionEvent, Terminal};

/// The epoch of a [`SessionEvent::Up`].
pub(crate) fn up(ev: &CoordEvent) -> Option<u32> {
    match ev {
        CoordEvent::Session(s) => match **s {
            SessionEvent::Up { epoch, .. } => Some(epoch),
            _ => None,
        },
        _ => None,
    }
}

/// A [`SessionEvent::Down`].
pub(crate) fn down(ev: &CoordEvent) -> bool {
    matches!(ev, CoordEvent::Session(s) if **s == SessionEvent::Down)
}

/// Where the terminal of `id` is in `log`, and what it is.
pub(crate) fn terminal(log: &[CoordEvent], id: RequestId) -> Option<(usize, &Terminal)> {
    log.iter().enumerate().find_map(|(i, e)| match e {
        CoordEvent::Done { id: d, t } if *d == id => Some((i, t)),
        _ => None,
    })
}

/// Where the progress of `id` is in `log`.
pub(crate) fn progress(log: &[CoordEvent], id: RequestId) -> Option<(usize, Progress)> {
    log.iter().enumerate().find_map(|(i, e)| match e {
        CoordEvent::Progress { id: d, p } if *d == id => Some((i, *p)),
        _ => None,
    })
}

/// The first event `pred` holds for.
pub(crate) fn position(log: &[CoordEvent], pred: impl Fn(&CoordEvent) -> bool) -> Option<usize> {
    log.iter().position(pred)
}

/// The request contract on a complete log of the `submitted` requests:
///
/// - exactly one terminal per request, none for requests never submitted;
/// - at most one progress, before the terminal;
/// - sessions alternate: no second `Up` without a `Down` between.
pub(crate) fn check(log: &[CoordEvent], submitted: &[RequestId]) -> Result<(), String> {
    for id in submitted {
        let terminals = log
            .iter()
            .filter(|e| matches!(e, CoordEvent::Done { id: d, .. } if d == id))
            .count();
        if terminals != 1 {
            return Err(format!("request {} has {terminals} terminals", id.0));
        }
        let progresses: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, CoordEvent::Progress { id: d, .. } if d == id))
            .map(|(i, _)| i)
            .collect();
        let (t, _) = terminal(log, *id).expect("counted above");
        match progresses[..] {
            [] => {}
            [p] if p < t => {}
            _ => {
                return Err(format!(
                    "request {}: progress at {progresses:?}, terminal at {t}",
                    id.0
                ));
            }
        }
    }
    for e in log {
        if let CoordEvent::Done { id, .. } | CoordEvent::Progress { id, .. } = e
            && !submitted.contains(id)
        {
            return Err(format!("event for a request never submitted: {e:?}"));
        }
    }
    let mut is_up = false;
    for e in log {
        if let Some(epoch) = up(e) {
            if is_up {
                return Err(format!("Up {epoch} without Down"));
            }
            is_up = true;
        } else if down(e) {
            is_up = false;
        }
    }
    Ok(())
}
