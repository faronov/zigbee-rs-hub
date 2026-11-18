//! esptool auto-reset semantics.
//!
//! esptool drives a "classic" USB-UART auto-reset circuit: two cross-coupled
//! transistors where EN is pulled low only while RTS is asserted and DTR is
//! not, and the boot strap (IO0, GPIO9 on the H2) only while DTR is asserted
//! and RTS is not. Both asserted or both released leave the target running,
//! which is why opening a port (both asserted) or closing it (both released)
//! does not reset it. The bridge reproduces that truth table on H2 EN/BOOT.

/// Target line levels derived from the host's modem control lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetLines {
    /// EN held low (target in reset).
    pub en_low: bool,
    /// BOOT strap held low (ROM download mode if sampled at reset release).
    pub boot_low: bool,
}

/// Classic esptool mapping: `EN = !(RTS && !DTR)`, `BOOT = !(DTR && !RTS)`.
pub const fn esptool_lines(dtr: bool, rts: bool) -> TargetLines {
    TargetLines {
        en_low: rts && !dtr,
        boot_low: dtr && !rts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: TargetLines = TargetLines { en_low: false, boot_low: false };

    #[test]
    fn idle_states_do_not_touch_the_target() {
        assert_eq!(esptool_lines(false, false), RUN);
        assert_eq!(esptool_lines(true, true), RUN);
    }

    #[test]
    fn classic_reset_enters_download_mode() {
        // esptool ClassicReset / UnixTightReset: (DTR,RTS) = (0,1) → (1,0) → (0,0).
        let reset = esptool_lines(false, true);
        assert!(reset.en_low && !reset.boot_low);
        let strap = esptool_lines(true, false);
        assert!(!strap.en_low && strap.boot_low);
        assert_eq!(esptool_lines(false, false), RUN);
    }

    #[test]
    fn hard_reset_after_flashing_boots_normally() {
        // esptool HardReset: RTS pulse with DTR released.
        assert_eq!(esptool_lines(false, true), TargetLines { en_low: true, boot_low: false });
        assert_eq!(esptool_lines(false, false), RUN);
    }
}
