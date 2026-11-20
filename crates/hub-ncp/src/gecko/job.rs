//! Radio update job: the steps of an update through the Gecko bootloader
//! across host reboots, as a pure state machine (the firmware keeps the
//! [`Job`] in NVS).
//!
//! 1. An upload lands in a radio bank as [`BankState::Staged`]
//!    ([`bank`](super::bank)).
//! 2. `Install { bank }` — before the hub starts, the host enters the
//!    bootloader and uploads the bank ([`install`](super::install)).
//! 3. `Verify { bank }` — the hub starts; the image is good when a ZRHP
//!    session comes up and its `build_id` matches the image's metadata.
//!    It is then promoted to last-known-good.
//! 4. A failed install or verification marks the bank bad and installs the
//!    last-known-good bank (`rollback`). A failed rollback ends the job
//!    with an error (no loops): the NCP is left in its bootloader (it has
//!    no valid application) or runs whatever it runs, and the operator
//!    uploads another image.

use super::bank::{BankState, Slot, bank_name};

/// The pending step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Install `bank` through the bootloader before the hub starts.
    Install {
        /// Bank.
        bank: u8,
        /// This is the rollback to the last-known-good bank.
        rollback: bool,
    },
    /// Wait for a verified session with `bank`'s image.
    Verify {
        /// Bank.
        bank: u8,
        /// This is the rollback to the last-known-good bank.
        rollback: bool,
    },
}

impl Job {
    /// NVS text: `install:A`, `verify:B:rollback`.
    pub fn encode(&self) -> String {
        let (step, bank, rb) = match *self {
            Job::Install { bank, rollback } => ("install", bank, rollback),
            Job::Verify { bank, rollback } => ("verify", bank, rollback),
        };
        format!("{step}:{}{}", bank_name(bank), if rb { ":rollback" } else { "" })
    }

    /// Parse [`Job::encode`].
    pub fn decode(s: &str) -> Option<Self> {
        let mut it = s.split(':');
        let step = it.next()?;
        let bank = match it.next()? {
            "A" => 0,
            "B" => 1,
            _ => return None,
        };
        let rollback = match it.next() {
            None => false,
            Some("rollback") => true,
            Some(_) => return None,
        };
        if it.next().is_some() {
            return None;
        }
        match step {
            "install" => Some(Job::Install { bank, rollback }),
            "verify" => Some(Job::Verify { bank, rollback }),
            _ => None,
        }
    }

    /// The bank the job is about.
    pub fn bank(&self) -> u8 {
        match *self {
            Job::Install { bank, .. } | Job::Verify { bank, .. } => bank,
        }
    }
}

/// What happened to the current step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The bootloader accepted the image.
    Installed,
    /// Bootloader entry or upload failed.
    InstallFailed,
    /// A session came up with the expected image.
    Verified,
    /// No session, or the wrong image.
    VerifyFailed,
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// The next job (`None`: done, clear it).
    pub job: Option<Job>,
    /// Record a bank state.
    pub mark: Option<(u8, BankState)>,
    /// Promote this bank to last-known-good.
    pub promote: Option<u8>,
    /// The job ended without a working new image.
    pub failed: bool,
}

/// The next step after `outcome` of `job`; `lkg` is the current
/// last-known-good bank.
pub fn next(job: Job, outcome: Outcome, lkg: Option<u8>) -> Step {
    let rollback_to = |bank: u8, rollback: bool| match lkg {
        Some(l) if !rollback && l != bank => Some(Job::Install { bank: l, rollback: true }),
        _ => None,
    };
    match (job, outcome) {
        (Job::Install { bank, rollback }, Outcome::Installed) => {
            Step { job: Some(Job::Verify { bank, rollback }), ..Step::default() }
        }
        (Job::Install { bank, rollback }, Outcome::InstallFailed) => Step {
            job: rollback_to(bank, rollback),
            mark: (!rollback).then_some((bank, BankState::Bad)),
            failed: true,
            ..Step::default()
        },
        (Job::Verify { bank, rollback }, Outcome::Verified) => {
            // A verified rollback leaves the bank good as it was.
            Step { promote: (!rollback).then_some(bank), failed: rollback, ..Step::default() }
        }
        (Job::Verify { bank, rollback }, Outcome::VerifyFailed) => Step {
            job: rollback_to(bank, rollback),
            mark: (!rollback).then_some((bank, BankState::Bad)),
            failed: true,
            ..Step::default()
        },
        // Out-of-order outcomes keep the job.
        (job, _) => Step { job: Some(job), ..Step::default() },
    }
}

/// Result of comparing the running NCP with the installed image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// `build_id` matches the image's zigbee-rs metadata.
    Match,
    /// The image has no zigbee-rs metadata: a session is all that can be
    /// checked.
    Unverifiable,
    /// A different image is running.
    Mismatch,
}

/// Compare the session's `SYS_NEGOTIATE.build_id` with the bank's image.
pub fn identity(slot: &Slot, build_id: &[u8; 8]) -> Identity {
    match slot.build_id {
        Some(b) if &b == build_id => Identity::Match,
        Some(_) => Identity::Mismatch,
        None => Identity::Unverifiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_text_round_trip() {
        for job in [
            Job::Install { bank: 0, rollback: false },
            Job::Install { bank: 1, rollback: true },
            Job::Verify { bank: 1, rollback: false },
            Job::Verify { bank: 0, rollback: true },
        ] {
            assert_eq!(Job::decode(&job.encode()), Some(job));
        }
        assert_eq!(Job::Verify { bank: 1, rollback: true }.encode(), "verify:B:rollback");
        for bad in ["", "install", "install:C", "verify:A:x", "verify:A:rollback:1", "run:A"] {
            assert_eq!(Job::decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn update_verified_promotes() {
        let s = next(Job::Install { bank: 1, rollback: false }, Outcome::Installed, Some(0));
        assert_eq!(s.job, Some(Job::Verify { bank: 1, rollback: false }));
        let s = next(Job::Verify { bank: 1, rollback: false }, Outcome::Verified, Some(0));
        assert_eq!(s, Step { promote: Some(1), ..Step::default() });
    }

    #[test]
    fn failures_roll_back_once() {
        let s = next(Job::Verify { bank: 1, rollback: false }, Outcome::VerifyFailed, Some(0));
        assert_eq!(s.job, Some(Job::Install { bank: 0, rollback: true }));
        assert_eq!(s.mark, Some((1, BankState::Bad)));
        assert!(s.failed);
        let s = next(Job::Install { bank: 1, rollback: false }, Outcome::InstallFailed, Some(0));
        assert_eq!(s.job, Some(Job::Install { bank: 0, rollback: true }));

        // The rollback itself failing ends the job, without touching the
        // last-known-good bank.
        let s = next(Job::Install { bank: 0, rollback: true }, Outcome::InstallFailed, Some(0));
        assert_eq!(s, Step { failed: true, ..Step::default() });
        let s = next(Job::Verify { bank: 0, rollback: true }, Outcome::VerifyFailed, Some(0));
        assert_eq!(s, Step { failed: true, ..Step::default() });
        // A verified rollback: done, the update still failed.
        let s = next(Job::Verify { bank: 0, rollback: true }, Outcome::Verified, Some(0));
        assert_eq!(s, Step { failed: true, ..Step::default() });

        // No last-known-good image (first update over the stock firmware).
        let s = next(Job::Verify { bank: 0, rollback: false }, Outcome::VerifyFailed, None);
        assert_eq!(s, Step { mark: Some((0, BankState::Bad)), failed: true, ..Step::default() });
    }

    #[test]
    fn identity_uses_the_build_id() {
        let slot = Slot {
            state: BankState::Staged,
            seq: 0,
            len: 1,
            crc32: 0,
            app_version: Some(1),
            build_id: Some(*b"build-01"),
        };
        assert_eq!(identity(&slot, b"build-01"), Identity::Match);
        assert_eq!(identity(&slot, b"build-02"), Identity::Mismatch);
        assert_eq!(identity(&Slot { build_id: None, ..slot }, b"build-02"), Identity::Unverifiable);
    }
}
