//! Vendor quirks, expressed as **data**.
//!
//! The transport in [`crate::bot`] and the command set in [`crate::scsi`] are
//! generic. Real card readers, however, deviate from the spec: some need a
//! vendor control transfer before they will answer SCSI, some report card
//! presence through a vendor command or an interrupt endpoint, some lie about
//! removability. Those deviations live here as a lookup table keyed by USB
//! VID/PID, so the generic path stays simple and auditable and adding a vendor
//! is adding a row — not a new branch in the hot path.
//!
//! This is the mechanism that lets the project grow beyond one chip: Realtek,
//! Genesys Logic, Alcor, … all slot in here.

/// How (if at all) the device must be brought up before SCSI works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitKind {
    /// Standards-compliant: nothing special.
    None,
    /// Needs a vendor-specific control transfer before it answers SCSI.
    /// Xthe exact payload is filled in per-device once reverse-engineered.
    Vendor,
}

/// How card presence is observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardDetect {
    /// Standard: poll TEST UNIT READY and inspect REQUEST SENSE.
    Scsi,
    /// Device signals changes on an interrupt endpoint.
    Interrupt,
    /// A vendor-specific status command (opcode recorded per device).
    VendorStatus,
}

/// How logical units map to physical slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LunModel {
    /// Honour `GET_MAX_LUN`.
    Standard,
    /// Device does not implement `GET_MAX_LUN`; assume this many LUNs.
    Fixed(u8),
    /// Several physical slots (xD/SD/MS) presented as one merged LUN.
    MergedSlots,
}

/// Behavioural overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuirkFlags(pub u32);

impl QuirkFlags {
    pub const NONE: Self = Self(0);
    /// Force the INQUIRY RMB ("removable media") bit on.
    pub const RMB_OVERRIDE: Self = Self(1 << 0);
    /// Skip `GET_MAX_LUN`; combine with [`LunModel::Fixed`].
    pub const NO_GET_MAX_LUN: Self = Self(1 << 1);
    /// Treat "medium not present" sense as a card removal immediately.
    pub const FAST_MEDIA_NOT_PRESENT: Self = Self(1 << 2);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One vendor's deviation from the standard.
#[derive(Debug, Clone, Copy)]
pub struct Quirk {
    pub vid: u16,
    /// `None` means "any product id from this vendor".
    pub pid: Option<u16>,
    pub name: &'static str,
    pub init: InitKind,
    pub card_detect: CardDetect,
    pub lun: LunModel,
    pub flags: QuirkFlags,
}

/// The behaviour of a device with no matching quirk: pure spec.
pub const DEFAULT_QUIRK: Quirk = Quirk {
    vid: 0,
    pid: None,
    name: "generic BOT/SCSI reader",
    init: InitKind::None,
    card_detect: CardDetect::Scsi,
    lun: LunModel::Standard,
    flags: QuirkFlags::NONE,
};

/// The catalogue. Most-specific match wins (see [`lookup`]).
pub static QUIRKS: &[Quirk] = &[
    // --- Realtek -------------------------------------------------------------
    // RTS5129 (IdeaPad 110 / 80UD): vendor class FF, BOT+SCSI underneath, but
    // the inbox USBSTOR driver cannot start it, so it needs vendor init. The
    // exact init sequence and card-detect mechanism are provided by the M1
    // probe; until then we record the structural facts.
    Quirk {
        vid: 0x0BDA,
        pid: Some(0x0129),
        name: "Realtek RTS5129 USB 2.0 Card Reader",
        init: InitKind::Vendor,
        card_detect: CardDetect::Scsi,
        lun: LunModel::MergedSlots,
        flags: QuirkFlags(QuirkFlags::RMB_OVERRIDE.0 | QuirkFlags::FAST_MEDIA_NOT_PRESENT.0),
    },
    // Other Realtek readers share the family behaviour.
    Quirk {
        vid: 0x0BDA,
        pid: None,
        name: "Realtek USB card reader (family)",
        init: InitKind::Vendor,
        card_detect: CardDetect::Scsi,
        lun: LunModel::Standard,
        flags: QuirkFlags::NONE,
    },
];

/// Find the best quirk for a device.
///
/// Precedence: exact VID+PID, then VID-only, then [`DEFAULT_QUIRK`].
#[must_use]
pub fn lookup(vid: u16, pid: u16) -> &'static Quirk {
    if let Some(q) = QUIRKS.iter().find(|q| q.vid == vid && q.pid == Some(pid)) {
        return q;
    }
    if let Some(q) = QUIRKS.iter().find(|q| q.vid == vid && q.pid.is_none()) {
        return q;
    }
    &DEFAULT_QUIRK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_beats_family() {
        let q = lookup(0x0BDA, 0x0129);
        assert_eq!(q.name, "Realtek RTS5129 USB 2.0 Card Reader");
        assert!(q.flags.contains(QuirkFlags::RMB_OVERRIDE));
    }

    #[test]
    fn family_match_for_unknown_pid() {
        let q = lookup(0x0BDA, 0x9999);
        assert_eq!(q.name, "Realtek USB card reader (family)");
        assert_eq!(q.init, InitKind::Vendor);
    }

    #[test]
    fn unknown_vendor_falls_back_to_spec() {
        let q = lookup(0x1234, 0x5678);
        assert_eq!(q.name, DEFAULT_QUIRK.name);
        assert_eq!(q.init, InitKind::None);
        assert_eq!(q.card_detect, CardDetect::Scsi);
    }

    #[test]
    fn flags_are_checked_correctly() {
        assert!(QuirkFlags::RMB_OVERRIDE.contains(QuirkFlags::RMB_OVERRIDE));
        assert!(!QuirkFlags::NONE.contains(QuirkFlags::RMB_OVERRIDE));
    }
}
