//! The bounded event queue between a backend worker and the application
//! thread (design §3): preallocated slots, a worker that waits while the
//! queue is full, and a doorbell that wakes the application at most once
//! until it drains the queue.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use crate::CoordEvent;

/// Slots of the application's event queue, and the most events it takes
/// per doorbell.
pub const EVENT_SLOTS: usize = 64;

/// Wakes the consumer (the application queues an input for itself). Must
/// not block.
pub type Doorbell = Box<dyn Fn() + Send + Sync>;

/// A bounded queue of [`CoordEvent`]s.
pub struct EventQueue {
    state: Mutex<State>,
    space: Condvar,
    rung: AtomicBool,
    doorbell: Doorbell,
    capacity: usize,
}

struct State {
    events: VecDeque<CoordEvent>,
    closed: bool,
    high_water: usize,
}

impl EventQueue {
    /// A queue of `capacity` slots (allocated now) that rings `doorbell`.
    pub fn new(capacity: usize, doorbell: Doorbell) -> Self {
        Self {
            state: Mutex::new(State {
                events: VecDeque::with_capacity(capacity),
                closed: false,
                high_water: 0,
            }),
            space: Condvar::new(),
            rung: AtomicBool::new(false),
            doorbell,
            capacity: capacity.max(1),
        }
    }

    /// Worker side: appends `ev`, waiting while the queue is full, and
    /// rings the doorbell unless it is still rung. `false`: the queue is
    /// closed (the event is dropped).
    pub fn push(&self, ev: CoordEvent) -> bool {
        let mut s = self.state.lock().unwrap();
        while s.events.len() >= self.capacity && !s.closed {
            s = self.space.wait(s).unwrap();
        }
        if s.closed {
            return false;
        }
        s.events.push_back(ev);
        s.high_water = s.high_water.max(s.events.len());
        drop(s);
        self.ring();
        true
    }

    fn ring(&self) {
        if !self.rung.swap(true, Ordering::SeqCst) {
            (self.doorbell)();
        }
    }

    /// Application side, on the doorbell: hands up to `max` events to `f`,
    /// in order, and rings again when more remain. Returns how many.
    pub fn drain(&self, max: usize, mut f: impl FnMut(CoordEvent)) -> usize {
        // Cleared first: an event pushed from now on rings again.
        self.rung.store(false, Ordering::SeqCst);
        let mut n = 0;
        while n < max {
            let ev = {
                let mut s = self.state.lock().unwrap();
                let ev = s.events.pop_front();
                if ev.is_some() {
                    self.space.notify_one();
                }
                ev
            };
            let Some(ev) = ev else { return n };
            f(ev);
            n += 1;
        }
        if !self.state.lock().unwrap().events.is_empty() {
            self.ring();
        }
        n
    }

    /// No consumer any more: queued events are dropped, a waiting worker
    /// returns and later pushes fail.
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        s.events.clear();
        self.space.notify_all();
    }

    /// Whether [`Self::close`] was called.
    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Events queued now.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().events.len()
    }

    /// Whether no event is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Most events queued at once.
    pub fn high_water(&self) -> usize {
        self.state.lock().unwrap().high_water
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn ev(n: u32) -> CoordEvent {
        CoordEvent::PermitJoin { remaining_s: n }
    }

    fn queue(capacity: usize) -> (Arc<EventQueue>, Arc<AtomicUsize>) {
        let rings = Arc::new(AtomicUsize::new(0));
        let r = rings.clone();
        let q = EventQueue::new(
            capacity,
            Box::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            }),
        );
        (Arc::new(q), rings)
    }

    #[test]
    fn one_doorbell_until_drained_and_again_while_events_remain() {
        let (q, rings) = queue(8);
        for n in 0..5 {
            assert!(q.push(ev(n)));
        }
        assert_eq!(rings.load(Ordering::SeqCst), 1);
        let mut got = Vec::new();
        assert_eq!(q.drain(3, |e| got.push(e)), 3);
        assert_eq!(got, [ev(0), ev(1), ev(2)]);
        // Two left: rung again.
        assert_eq!(rings.load(Ordering::SeqCst), 2);
        assert_eq!(q.drain(3, |e| got.push(e)), 2);
        assert_eq!(rings.load(Ordering::SeqCst), 2);
        assert!(q.push(ev(9)));
        assert_eq!(rings.load(Ordering::SeqCst), 3);
        assert_eq!(q.high_water(), 5);
    }

    #[test]
    fn a_full_queue_holds_the_worker_until_the_app_drains_it() {
        let (q, _) = queue(2);
        let w = q.clone();
        let worker = std::thread::spawn(move || (0..5).all(|n| w.push(ev(n))));
        let mut got = Vec::new();
        while got.len() < 5 {
            q.drain(EVENT_SLOTS, |e| got.push(e));
            assert!(q.len() <= 2);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worker.join().unwrap());
        assert_eq!(got, (0..5).map(ev).collect::<Vec<_>>());
    }

    #[test]
    fn closing_releases_a_waiting_worker() {
        let (q, _) = queue(1);
        assert!(q.push(ev(0)));
        let w = q.clone();
        let worker = std::thread::spawn(move || w.push(ev(1)));
        std::thread::sleep(Duration::from_millis(20));
        q.close();
        assert!(!worker.join().unwrap());
        assert!(q.is_empty() && q.is_closed());
        assert!(!q.push(ev(2)));
    }
}
