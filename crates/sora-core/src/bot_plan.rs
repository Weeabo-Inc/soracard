//! **BOT transaction planner / state machine.**
//!
//! [`crate::bot`] knows how to *frame* a Bulk-Only Transport command: it builds
//! a CBW and parses a CSW. This module adds the missing piece the driver needs:
//! the *sequence* — which phase comes next, how many bytes still belong to the
//! data phase, whether the tag matches, and what the CSW's status + residue mean
//! for the OS.
//!
//! One [`BotTransaction`] is one SCSI command in flight:
//!
//! ```text
//!   SendCbw ──▶ DataOut(n) / DataIn(n) ──▶ ReadCsw ──▶ Done(Succeeded|Failed)
//!      │                 │                     │
//!      └─────────────────┴─────────────────────┴──▶ PhaseError
//! ```
//!
//! The driver repeatedly asks [`BotTransaction::state`] what to do and reports
//! back what happened via [`BotTransaction::advance`]. Every transition is a
//! pure function of the prior state and the observed event, so the whole thing
//! is exercised on the host without a USB stack.
//!
//! ## Residue, short transfers and recovery
//!
//! A CSW may carry a non-zero **residue** (bytes the device did *not* transfer).
//! For a command that expected data this is a short transfer and the request
//! cannot be reported as fully satisfied. [`MappingReport`] turns the raw
//! `(status, residue)` into a transport-agnostic [`Outcome`] plus two recovery
//! hints:
//!
//! * [`MappingReport::requires_sense`] — a command failed, so the driver should
//!   issue a standard `REQUEST SENSE` to learn why (BOT rev 1.0, §6.7).
//! * [`MappingReport::requires_reset_recovery`] — the transport hit a phase
//!   error; the driver must run the BOT reset recovery before anything else
//!   (`Mass Storage Reset`, then clear both bulk endpoints), and only then may
//!   it issue `REQUEST SENSE`.
//!
//! The state machine never performs I/O; it only decides.

use crate::bot::{self, BotError, Cbw, Csw, CswStatus, Direction};

/// Data-phase direction, including the "no data" case SCSI commands need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirection {
    /// No data phase (e.g. TEST UNIT READY, START STOP UNIT).
    None,
    /// Host -> device (WRITE).
    Out,
    /// Device -> host (READ, INQUIRY, …).
    In,
}

/// Why a [`BotTransaction`] could not be constructed.
///
/// Invalid CDBs and LUNs are programmer errors in the SCSI layer, but a driver
/// still prefers a `Result` over a panic on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// The CDB slice was empty.
    EmptyCdb,
    /// The CDB slice exceeded [`bot::MAX_CDB_LEN`] (16) bytes.
    CdbTooLong,
    /// The LUN was greater than 15.
    InvalidLun,
    /// A non-zero data length was given with [`DataDirection::None`].
    LengthWithoutDirection,
}

/// How a CSW turned out, in a form the driver can map onto an SRB status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The device reported the command passed and transferred everything.
    Succeeded,
    /// The command failed (CSW status `Failed`, or a short transfer).
    /// `REQUEST SENSE` should follow.
    Failed,
    /// BOT phase error: bad CSW, tag mismatch, or CSW status `PhaseError`.
    /// The driver must perform reset recovery before reusing the endpoints.
    PhaseError,
}

/// Terminal success/failure carried by [`BotState::Done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Succeeded,
    Failed,
}

/// The machine cursor. Each variant is exactly what the driver does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotState {
    /// Send the [`BotTransaction::cbw_bytes`] (31 bytes) on bulk OUT.
    SendCbw,
    /// Send `n` more bytes of data on bulk OUT.
    DataOut(u32),
    /// Receive `n` more bytes of data on bulk IN.
    DataIn(u32),
    /// Receive exactly [`BotTransaction::expected_csw_len`] bytes on bulk IN.
    ReadCsw,
    /// Terminal: the command completed (with success or a reportable failure).
    Done(Completion),
    /// Terminal: the transport is wedged; reset recovery is required.
    PhaseError,
}

/// An event the driver observes and feeds back to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotEvent<'a> {
    /// The 31-byte CBW was written in full.
    CbwSent,
    /// `bytes` more bytes moved in the current data phase.
    DataTransferred { bytes: u32 },
    /// The data phase stalled, timed out, or was cancelled.
    DataFailed,
    /// Raw CSW bytes arrived (may be short or malformed; parsing is delegated
    /// to [`bot::parse_csw`]).
    CswReceived(&'a [u8]),
}

/// Why an event could not be consumed as given.
///
/// A returned error is not necessarily fatal: malformed CSWs, tag mismatches
/// and overruns still move the machine to [`BotState::PhaseError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionError {
    /// The event does not apply in the current state (e.g. data before CBW).
    WrongState(BotState),
    /// More bytes arrived than the data phase expected.
    Overrun { expected: u32, got: u32 },
    /// The CSW could not be parsed.
    Csw(BotError),
    /// The CSW's tag did not match this transaction's tag.
    TagMismatch { expected: u32, got: u32 },
}

/// Monotonic BOT tag source. Tags are never zero, and wrap from `u32::MAX`
/// back to `1` (zero is skipped to keep it usable as a sentinel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagGenerator {
    next: u32,
}

impl TagGenerator {
    /// Create a generator. A `seed` of 0 is normalised to 1.
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self {
            next: if seed == 0 { 1 } else { seed },
        }
    }

    /// Return the next tag and advance.
    pub fn next_tag(&mut self) -> u32 {
        let tag = self.next;
        let following = self.next.wrapping_add(1);
        self.next = if following == 0 { 1 } else { following };
        tag
    }
}

impl Default for TagGenerator {
    fn default() -> Self {
        Self::new(1)
    }
}

/// A CSW distilled into something the OS-facing layer can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingReport {
    /// Raw CSW status.
    pub csw_status: CswStatus,
    /// Bytes the device reported as *not* transferred.
    pub residue: u32,
    /// Data length the CBW asked for.
    pub expected: u32,
    /// Bytes actually transferred (`expected - residue`, saturating).
    pub transferred: u32,
    /// Whether the CSW tag matched the outstanding CBW.
    pub tag_matched: bool,
    /// A data phase was expected but the device transferred less than asked.
    pub short_transfer: bool,
    /// The generic outcome to translate into an SRB status.
    pub outcome: Outcome,
}

impl MappingReport {
    /// Map a parsed CSW against the outstanding tag and expected length.
    #[must_use]
    pub const fn from_csw(csw: &Csw, expected_tag: u32, expected_len: u32) -> Self {
        let tag_matched = csw.tag == expected_tag;
        let overrun = csw.residue > expected_len;
        let short_transfer = expected_len > 0 && csw.residue > 0 && !overrun;
        let outcome = if tag_matched {
            match csw.status {
                CswStatus::PhaseError => Outcome::PhaseError,
                CswStatus::Failed => Outcome::Failed,
                CswStatus::Passed => {
                    if overrun {
                        // A residue larger than the transfer is protocol nonsense.
                        Outcome::PhaseError
                    } else if csw.residue > 0 {
                        // Short transfer: not everything the OS asked for moved.
                        Outcome::Failed
                    } else {
                        Outcome::Succeeded
                    }
                }
            }
        } else {
            Outcome::PhaseError
        };
        Self {
            csw_status: csw.status,
            residue: csw.residue,
            expected: expected_len,
            transferred: expected_len.saturating_sub(csw.residue),
            tag_matched,
            short_transfer,
            outcome,
        }
    }

    /// True when the residue exceeds the requested length (a protocol error).
    #[must_use]
    pub const fn overrun(&self) -> bool {
        self.residue > self.expected
    }

    /// True when a `REQUEST SENSE` should follow this command
    /// (any non-success outcome, including a short transfer).
    #[must_use]
    pub const fn requires_sense(&self) -> bool {
        match self.outcome {
            Outcome::Succeeded => false,
            Outcome::Failed | Outcome::PhaseError => true,
        }
    }

    /// True when the BOT reset recovery must run before the next command.
    #[must_use]
    pub const fn requires_reset_recovery(&self) -> bool {
        match self.outcome {
            Outcome::PhaseError => true,
            Outcome::Succeeded | Outcome::Failed => false,
        }
    }
}

/// One BOT command in flight: its immutable framing plus a mutable cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotTransaction {
    tag: u32,
    lun: u8,
    direction: DataDirection,
    data_len: u32,
    cdb: [u8; bot::MAX_CDB_LEN],
    cdb_len: u8,

    state: BotState,
    remaining: u32,
    report: Option<MappingReport>,
}

impl BotTransaction {
    /// Plan a transaction for `cdb`.
    ///
    /// `direction`/`data_len` describe the SCSI data phase. A `data_len` of 0
    /// (or [`DataDirection::None`]) means there is no data phase and the CBW's
    /// transfer length is 0.
    ///
    /// # Errors
    /// Returns [`PlanError`] for an empty/oversized CDB, an out-of-range LUN,
    /// or a non-zero length with [`DataDirection::None`].
    #[allow(clippy::cast_possible_truncation)] // cdb.len() <= MAX_CDB_LEN == 16
    pub fn new(
        tag: u32,
        lun: u8,
        direction: DataDirection,
        data_len: u32,
        cdb: &[u8],
    ) -> Result<Self, PlanError> {
        if cdb.is_empty() {
            return Err(PlanError::EmptyCdb);
        }
        if cdb.len() > bot::MAX_CDB_LEN {
            return Err(PlanError::CdbTooLong);
        }
        if lun > 15 {
            return Err(PlanError::InvalidLun);
        }
        if matches!(direction, DataDirection::None) && data_len != 0 {
            return Err(PlanError::LengthWithoutDirection);
        }

        let mut cdb_buf = [0u8; bot::MAX_CDB_LEN];
        cdb_buf[..cdb.len()].copy_from_slice(cdb);
        let remaining = if matches!(direction, DataDirection::None) {
            0
        } else {
            data_len
        };
        Ok(Self {
            tag,
            lun,
            direction,
            data_len,
            cdb: cdb_buf,
            cdb_len: cdb.len() as u8,
            state: BotState::SendCbw,
            remaining,
            report: None,
        })
    }

    #[must_use]
    pub const fn tag(&self) -> u32 {
        self.tag
    }

    #[must_use]
    pub const fn lun(&self) -> u8 {
        self.lun
    }

    #[must_use]
    pub const fn direction(&self) -> DataDirection {
        self.direction
    }

    #[must_use]
    pub const fn data_len(&self) -> u32 {
        self.data_len
    }

    /// The valid CDB bytes.
    #[must_use]
    pub fn cdb(&self) -> &[u8] {
        &self.cdb[..usize::from(self.cdb_len)]
    }

    /// True when a data phase is actually required (`data_len > 0`).
    #[must_use]
    pub const fn has_data_phase(&self) -> bool {
        self.data_len > 0 && !matches!(self.direction, DataDirection::None)
    }

    /// The data-phase direction and byte count, or `None` when there is none.
    #[must_use]
    pub const fn data_phase(&self) -> Option<(Direction, u32)> {
        if self.has_data_phase() {
            let dir = match self.direction {
                DataDirection::In => Direction::In,
                DataDirection::Out | DataDirection::None => Direction::Out,
            };
            Some((dir, self.data_len))
        } else {
            None
        }
    }

    /// The bytes that must be written on bulk OUT to start the command.
    ///
    /// When there is no data phase the direction flag is forced to OUT (0x00),
    /// as BOT requires, and the transfer length is 0.
    #[must_use]
    pub fn cbw(&self) -> Cbw {
        let direction = if self.has_data_phase() {
            match self.direction {
                DataDirection::In => Direction::In,
                DataDirection::Out | DataDirection::None => Direction::Out,
            }
        } else {
            Direction::Out
        };
        Cbw::new(self.tag, direction, self.lun, self.data_len, self.cdb())
    }

    /// Convenience wrapper around [`Cbw::encode`].
    #[must_use]
    pub fn cbw_bytes(&self) -> [u8; bot::CBW_LEN] {
        self.cbw().encode()
    }

    /// Number of bytes the CSW occupies (always [`bot::CSW_LEN`]).
    #[must_use]
    pub const fn expected_csw_len(&self) -> usize {
        bot::CSW_LEN
    }

    /// Current cursor — the driver's next action.
    #[must_use]
    pub const fn state(&self) -> BotState {
        self.state
    }

    /// Bytes still to move in the data phase.
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        self.remaining
    }

    /// The CSW mapping, available once a CSW has been parsed.
    #[must_use]
    pub const fn report(&self) -> Option<MappingReport> {
        self.report
    }

    /// The generic outcome, once the transaction has terminated.
    #[must_use]
    pub const fn outcome(&self) -> Option<Outcome> {
        match self.state {
            BotState::Done(Completion::Succeeded) => Some(Outcome::Succeeded),
            BotState::Done(Completion::Failed) => Some(Outcome::Failed),
            BotState::PhaseError => Some(Outcome::PhaseError),
            BotState::SendCbw | BotState::DataOut(_) | BotState::DataIn(_) | BotState::ReadCsw => {
                None
            }
        }
    }

    /// True once no further event can advance the machine.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.state, BotState::Done(_) | BotState::PhaseError)
    }

    /// True when the driver should issue `REQUEST SENSE` next.
    ///
    /// This is true for any failed or phase-errored transaction, including one
    /// that never produced a CSW (e.g. a stalled data phase), and false for a
    /// clean success. For a phase error, reset recovery comes first.
    #[must_use]
    pub const fn requires_sense(&self) -> bool {
        match self.state {
            BotState::Done(Completion::Failed) | BotState::PhaseError => true,
            BotState::SendCbw
            | BotState::DataOut(_)
            | BotState::DataIn(_)
            | BotState::ReadCsw
            | BotState::Done(Completion::Succeeded) => false,
        }
    }

    /// True when the driver must run BOT reset recovery before reusing the
    /// bulk endpoints.
    #[must_use]
    pub const fn requires_reset_recovery(&self) -> bool {
        matches!(self.state, BotState::PhaseError)
    }

    /// Feed one observed event and return the resulting state.
    ///
    /// # Errors
    /// Returns [`TransactionError`] when the event is out of order or the CSW
    /// is unusable. Parse failures, tag mismatches and overruns still leave the
    /// machine in [`BotState::PhaseError`]; the error is reported so the driver
    /// can log the exact reason.
    pub fn advance(&mut self, event: BotEvent<'_>) -> Result<BotState, TransactionError> {
        match event {
            BotEvent::CbwSent => self.on_cbw_sent(),
            BotEvent::DataTransferred { bytes } => self.on_data(bytes),
            BotEvent::DataFailed => self.on_data_failed(),
            BotEvent::CswReceived(bytes) => self.on_csw(bytes),
        }
    }

    fn on_cbw_sent(&mut self) -> Result<BotState, TransactionError> {
        if self.state != BotState::SendCbw {
            return Err(TransactionError::WrongState(self.state));
        }
        if self.has_data_phase() {
            self.remaining = self.data_len;
            self.state = match self.direction {
                DataDirection::In => BotState::DataIn(self.remaining),
                DataDirection::Out | DataDirection::None => BotState::DataOut(self.remaining),
            };
        } else {
            self.remaining = 0;
            self.state = BotState::ReadCsw;
        }
        Ok(self.state)
    }

    fn on_data(&mut self, bytes: u32) -> Result<BotState, TransactionError> {
        let remaining = match self.state {
            BotState::DataIn(n) | BotState::DataOut(n) => n,
            other => return Err(TransactionError::WrongState(other)),
        };
        if bytes > remaining {
            self.state = BotState::PhaseError;
            return Err(TransactionError::Overrun {
                expected: remaining,
                got: bytes,
            });
        }
        let left = remaining - bytes;
        self.remaining = left;
        self.state = if left == 0 {
            BotState::ReadCsw
        } else {
            match self.direction {
                DataDirection::In => BotState::DataIn(left),
                DataDirection::Out | DataDirection::None => BotState::DataOut(left),
            }
        };
        Ok(self.state)
    }

    fn on_data_failed(&mut self) -> Result<BotState, TransactionError> {
        match self.state {
            BotState::DataIn(_) | BotState::DataOut(_) => {
                self.state = BotState::PhaseError;
                Ok(self.state)
            }
            other => Err(TransactionError::WrongState(other)),
        }
    }

    fn on_csw(&mut self, bytes: &[u8]) -> Result<BotState, TransactionError> {
        if self.state != BotState::ReadCsw {
            return Err(TransactionError::WrongState(self.state));
        }
        let csw = match bot::parse_csw(bytes) {
            Ok(csw) => csw,
            Err(err) => {
                self.state = BotState::PhaseError;
                return Err(TransactionError::Csw(err));
            }
        };
        let report = MappingReport::from_csw(&csw, self.tag, self.data_len);
        self.report = Some(report);
        if !report.tag_matched {
            self.state = BotState::PhaseError;
            return Err(TransactionError::TagMismatch {
                expected: self.tag,
                got: csw.tag,
            });
        }
        self.state = match report.outcome {
            Outcome::Succeeded => BotState::Done(Completion::Succeeded),
            Outcome::Failed => BotState::Done(Completion::Failed),
            Outcome::PhaseError => BotState::PhaseError,
        };
        Ok(self.state)
    }
}

/// How a finished BOT command should be reported to the SCSI layer above.
///
/// This differs deliberately from [`MappingReport::outcome`]: in SCSI a short
/// data-in with a *passed* status is normal (an `INQUIRY` with allocation
/// length 255 answered with 36 bytes), so it is an *underrun*, not a failure,
/// and must not trigger `REQUEST SENSE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// CSW passed and every requested byte moved.
    Good,
    /// CSW passed but fewer bytes moved than requested.
    Underrun { transferred: u32 },
    /// CSW failed: report CHECK CONDITION and fetch sense.
    CheckCondition { transferred: u32 },
    /// The transport is out of sync (bad CSW, tag mismatch, phase error):
    /// run reset recovery; the command's outcome is unknown.
    PhaseError,
}

/// Map a received CSW onto a [`Disposition`].
///
/// * `raw_csw` — the bytes read in the status phase (may be short/garbage).
/// * `expected_len` — the CBW's `dCBWDataTransferLength`.
/// * `moved` — bytes the USB stack actually moved in the data phase.
///
/// The transferred count is the smaller of what USB moved and what the device
/// claims (`expected - residue`). A residue larger than the request is
/// nonsense some firmware produces; like Linux `usb-storage` we then ignore
/// the residue rather than wedge the transport over it.
#[must_use]
pub fn disposition(
    raw_csw: &[u8],
    expected_tag: u32,
    expected_len: u32,
    moved: u32,
) -> Disposition {
    if raw_csw.len() != bot::CSW_LEN {
        return Disposition::PhaseError;
    }
    let Ok(csw) = bot::parse_csw(raw_csw) else {
        return Disposition::PhaseError;
    };
    if csw.tag != expected_tag {
        return Disposition::PhaseError;
    }
    let moved = moved.min(expected_len);
    let claimed = if csw.residue > expected_len {
        moved
    } else {
        expected_len - csw.residue
    };
    let transferred = moved.min(claimed);
    match csw.status {
        CswStatus::PhaseError => Disposition::PhaseError,
        CswStatus::Failed => Disposition::CheckCondition { transferred },
        CswStatus::Passed if transferred == expected_len => Disposition::Good,
        CswStatus::Passed => Disposition::Underrun { transferred },
    }
}

#[cfg(test)]
mod disposition_tests {
    use super::*;

    fn csw(tag: u32, residue: u32, status: u8) -> [u8; bot::CSW_LEN] {
        let mut b = [0u8; bot::CSW_LEN];
        b[0..4].copy_from_slice(&bot::CSW_SIGNATURE.to_le_bytes());
        b[4..8].copy_from_slice(&tag.to_le_bytes());
        b[8..12].copy_from_slice(&residue.to_le_bytes());
        b[12] = status;
        b
    }

    #[test]
    fn full_transfer_is_good() {
        assert_eq!(disposition(&csw(5, 0, 0), 5, 512, 512), Disposition::Good);
        assert_eq!(disposition(&csw(5, 0, 0), 5, 0, 0), Disposition::Good);
    }

    #[test]
    fn short_inquiry_is_underrun_not_failure() {
        assert_eq!(
            disposition(&csw(9, 255 - 36, 0), 9, 255, 36),
            Disposition::Underrun { transferred: 36 }
        );
    }

    #[test]
    fn residue_limits_out_transfer_even_if_usb_moved_everything() {
        // A write where the device accepted the bytes but reports 512 unwritten.
        assert_eq!(
            disposition(&csw(1, 512, 0), 1, 4096, 4096),
            Disposition::Underrun { transferred: 3584 }
        );
    }

    #[test]
    fn failed_status_requests_sense_with_partial_count() {
        assert_eq!(
            disposition(&csw(2, 0, 1), 2, 0, 0),
            Disposition::CheckCondition { transferred: 0 }
        );
        assert_eq!(
            disposition(&csw(2, 1024, 1), 2, 4096, 3072),
            Disposition::CheckCondition { transferred: 3072 }
        );
    }

    #[test]
    fn bogus_residue_is_ignored() {
        assert_eq!(
            disposition(&csw(3, 0xFFFF_FFFF, 0), 3, 36, 36),
            Disposition::Good
        );
    }

    #[test]
    fn transport_desync_is_phase_error() {
        assert_eq!(disposition(&csw(3, 0, 0), 4, 0, 0), Disposition::PhaseError);
        assert_eq!(disposition(&csw(3, 0, 2), 3, 0, 0), Disposition::PhaseError);
        assert_eq!(disposition(&csw(3, 0, 7), 3, 0, 0), Disposition::PhaseError);
        assert_eq!(
            disposition(&csw(3, 0, 0)[..12], 3, 0, 0),
            Disposition::PhaseError
        );
        let mut bad = csw(3, 0, 0);
        bad[0] = 0;
        assert_eq!(disposition(&bad, 3, 0, 0), Disposition::PhaseError);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{BotError, Cbw, Csw, CswStatus, Direction};

    /// Build a valid on-the-wire CSW.
    fn csw_bytes(tag: u32, residue: u32, status: u8) -> [u8; bot::CSW_LEN] {
        let mut b = [0u8; bot::CSW_LEN];
        b[0..4].copy_from_slice(&bot::CSW_SIGNATURE.to_le_bytes());
        b[4..8].copy_from_slice(&tag.to_le_bytes());
        b[8..12].copy_from_slice(&residue.to_le_bytes());
        b[12] = status;
        b
    }

    fn inquiry() -> [u8; 6] {
        [0x12, 0x00, 0x00, 0x00, 36, 0x00]
    }

    fn read10() -> [u8; 10] {
        [0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00]
    }

    #[test]
    fn inquiry_in_happy_path() {
        let cdb = inquiry();
        let mut t = BotTransaction::new(0x0042, 0, DataDirection::In, 36, &cdb).unwrap();

        assert_eq!(t.state(), BotState::SendCbw);
        assert_eq!(t.expected_csw_len(), bot::CSW_LEN);
        assert_eq!(t.data_phase(), Some((Direction::In, 36)));
        assert_eq!(
            t.cbw_bytes(),
            Cbw::new(0x0042, Direction::In, 0, 36, &cdb).encode()
        );
        assert_eq!(t.cbw_bytes()[12], 0x80);
        assert_eq!(&t.cbw_bytes()[8..12], &36u32.to_le_bytes());
        assert_eq!(t.cdb(), &cdb);

        assert_eq!(t.advance(BotEvent::CbwSent).unwrap(), BotState::DataIn(36));
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap(),
            BotState::ReadCsw
        );
        assert_eq!(t.remaining(), 0);

        let csw = csw_bytes(0x0042, 0, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Succeeded)
        );
        assert!(t.is_terminal());
        assert_eq!(t.outcome(), Some(Outcome::Succeeded));

        let r = t.report().unwrap();
        assert!(r.tag_matched);
        assert!(!r.short_transfer);
        assert_eq!(r.transferred, 36);
        assert_eq!(r.outcome, Outcome::Succeeded);
        assert!(!t.requires_sense());
        assert!(!t.requires_reset_recovery());
    }

    #[test]
    fn write_out_happy_path_is_chunked() {
        let cdb = [0x2A, 0x00, 0x00, 0x00, 0x12, 0x34, 0x00, 0x00, 0x02, 0x00];
        let mut t = BotTransaction::new(7, 0, DataDirection::Out, 1024, &cdb).unwrap();

        assert_eq!(t.data_phase(), Some((Direction::Out, 1024)));
        assert_eq!(t.cbw_bytes()[12], 0x00, "OUT sets bmCBWFlags bit 7 to 0");

        assert_eq!(
            t.advance(BotEvent::CbwSent).unwrap(),
            BotState::DataOut(1024)
        );
        assert_eq!(t.remaining(), 1024);
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 512 }).unwrap(),
            BotState::DataOut(512)
        );
        assert_eq!(t.remaining(), 512);
        // A zero-length chunk is legal and makes no progress.
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 0 }).unwrap(),
            BotState::DataOut(512)
        );
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 512 }).unwrap(),
            BotState::ReadCsw
        );

        let csw = csw_bytes(7, 0, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Succeeded)
        );
        assert_eq!(t.report().unwrap().outcome, Outcome::Succeeded);
    }

    #[test]
    fn short_read_residue_is_failed() {
        let mut t = BotTransaction::new(1, 0, DataDirection::In, 512, &read10()).unwrap();
        assert_eq!(t.advance(BotEvent::CbwSent).unwrap(), BotState::DataIn(512));
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 512 }).unwrap(),
            BotState::ReadCsw
        );

        // Status passed but 100 bytes were never transferred.
        let csw = csw_bytes(1, 100, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Failed)
        );
        let r = t.report().unwrap();
        assert!(r.short_transfer);
        assert_eq!(r.residue, 100);
        assert_eq!(r.transferred, 412);
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(t.requires_sense());
        assert!(!t.requires_reset_recovery());
    }

    #[test]
    fn short_write_residue_is_failed() {
        let cdb = [0x2A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
        let mut t = BotTransaction::new(2, 0, DataDirection::Out, 512, &cdb).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        t.advance(BotEvent::DataTransferred { bytes: 512 }).unwrap();

        let csw = csw_bytes(2, 12, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Failed)
        );
        assert!(t.report().unwrap().short_transfer);
        assert!(t.requires_sense());
    }

    #[test]
    fn csw_failed_requires_sense_but_no_reset() {
        let mut t = BotTransaction::new(3, 0, DataDirection::In, 36, &inquiry()).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap();

        let csw = csw_bytes(3, 0, 1); // Failed
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Failed)
        );
        assert_eq!(t.outcome(), Some(Outcome::Failed));
        assert!(t.requires_sense());
        assert!(!t.requires_reset_recovery());
    }

    #[test]
    fn csw_phase_error_requires_reset_recovery() {
        let mut t = BotTransaction::new(4, 0, DataDirection::In, 36, &inquiry()).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap();

        let csw = csw_bytes(4, 0, 2); // PhaseError
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::PhaseError
        );
        assert_eq!(t.state(), BotState::PhaseError);
        assert_eq!(t.outcome(), Some(Outcome::PhaseError));
        assert!(t.is_terminal());
        assert!(t.requires_sense());
        assert!(t.requires_reset_recovery());
    }

    #[test]
    fn tag_mismatch_is_phase_error() {
        let mut t = BotTransaction::new(0xAA, 0, DataDirection::In, 36, &inquiry()).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap();

        let csw = csw_bytes(0xBB, 0, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)),
            Err(TransactionError::TagMismatch {
                expected: 0xAA,
                got: 0xBB,
            })
        );
        assert_eq!(t.state(), BotState::PhaseError);
        let r = t.report().unwrap();
        assert!(!r.tag_matched);
        assert_eq!(r.outcome, Outcome::PhaseError);
        assert!(t.requires_reset_recovery());
    }

    #[test]
    fn malformed_csw_is_phase_error() {
        let mut t = BotTransaction::new(5, 0, DataDirection::In, 36, &inquiry()).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap();

        assert_eq!(
            t.advance(BotEvent::CswReceived(&[0u8; 4])),
            Err(TransactionError::Csw(BotError::ShortCsw))
        );
        assert_eq!(t.state(), BotState::PhaseError);
        assert!(t.report().is_none());
        assert_eq!(t.outcome(), Some(Outcome::PhaseError));
    }

    #[test]
    fn bad_csw_signature_is_phase_error_without_report() {
        let mut t = BotTransaction::new(6, 0, DataDirection::None, 0, &[0, 0, 0, 0, 0, 0]).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        assert_eq!(
            t.advance(BotEvent::CswReceived(&[0xFFu8; bot::CSW_LEN])),
            Err(TransactionError::Csw(BotError::BadCswSignature))
        );
        assert_eq!(t.state(), BotState::PhaseError);
    }

    #[test]
    fn zero_length_data_skips_data_phase() {
        // TEST UNIT READY has no data, represented as None.
        let mut t =
            BotTransaction::new(9, 0, DataDirection::None, 0, &[0x00, 0, 0, 0, 0, 0]).unwrap();
        assert!(!t.has_data_phase());
        assert_eq!(t.data_phase(), None);
        assert_eq!(&t.cbw_bytes()[8..12], &0u32.to_le_bytes());
        assert_eq!(t.cbw_bytes()[12], 0x00);
        assert_eq!(t.advance(BotEvent::CbwSent).unwrap(), BotState::ReadCsw);
        let csw = csw_bytes(9, 0, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Succeeded)
        );

        // An IN command with a zero allocation also degenerates to no data.
        let mut t2 = BotTransaction::new(10, 0, DataDirection::In, 0, &inquiry()).unwrap();
        assert!(!t2.has_data_phase());
        assert_eq!(
            t2.cbw_bytes()[12],
            0x00,
            "no data phase forces bmCBWFlags to 0"
        );
        assert_eq!(t2.advance(BotEvent::CbwSent).unwrap(), BotState::ReadCsw);
        let csw = csw_bytes(10, 0, 0);
        assert_eq!(
            t2.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::Done(Completion::Succeeded)
        );
    }

    #[test]
    fn residue_without_data_phase_is_phase_error() {
        let mut t =
            BotTransaction::new(11, 0, DataDirection::None, 0, &[0, 0, 0, 0, 0, 0]).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        let csw = csw_bytes(11, 5, 0); // passed, but no data phase existed
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)).unwrap(),
            BotState::PhaseError
        );
        let r = t.report().unwrap();
        assert!(r.overrun());
        assert_eq!(r.outcome, Outcome::PhaseError);
        assert!(t.requires_reset_recovery());
    }

    #[test]
    fn data_overrun_is_phase_error() {
        let mut t = BotTransaction::new(12, 0, DataDirection::In, 100, &read10()).unwrap();
        t.advance(BotEvent::CbwSent).unwrap();
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 101 }),
            Err(TransactionError::Overrun {
                expected: 100,
                got: 101,
            })
        );
        assert_eq!(t.state(), BotState::PhaseError);
    }

    #[test]
    fn data_phase_stall_is_phase_error() {
        let cdb = [0x2A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
        let mut t = BotTransaction::new(13, 0, DataDirection::Out, 512, &cdb).unwrap();
        assert_eq!(
            t.advance(BotEvent::CbwSent).unwrap(),
            BotState::DataOut(512)
        );
        assert_eq!(
            t.advance(BotEvent::DataFailed).unwrap(),
            BotState::PhaseError
        );
        assert!(t.requires_reset_recovery());
    }

    #[test]
    fn wrong_state_events_are_rejected() {
        let mut t = BotTransaction::new(14, 0, DataDirection::In, 36, &inquiry()).unwrap();
        assert_eq!(
            t.advance(BotEvent::DataTransferred { bytes: 1 }),
            Err(TransactionError::WrongState(BotState::SendCbw))
        );
        assert_eq!(
            t.advance(BotEvent::DataFailed),
            Err(TransactionError::WrongState(BotState::SendCbw))
        );

        t.advance(BotEvent::CbwSent).unwrap();
        // The CSW cannot arrive before the data phase is complete.
        let csw = csw_bytes(14, 0, 0);
        assert_eq!(
            t.advance(BotEvent::CswReceived(&csw)),
            Err(TransactionError::WrongState(BotState::DataIn(36)))
        );

        t.advance(BotEvent::DataTransferred { bytes: 36 }).unwrap();
        t.advance(BotEvent::CswReceived(&csw)).unwrap();
        // Terminal states absorb nothing.
        assert_eq!(
            t.advance(BotEvent::CbwSent),
            Err(TransactionError::WrongState(BotState::Done(
                Completion::Succeeded
            )))
        );
    }

    #[test]
    fn mapping_report_maps_status_and_residue() {
        let passed = Csw {
            tag: 5,
            residue: 0,
            status: CswStatus::Passed,
        };
        let r = MappingReport::from_csw(&passed, 5, 512);
        assert_eq!(r.outcome, Outcome::Succeeded);
        assert!(!r.requires_sense());
        assert!(!r.short_transfer);

        let passed_short = Csw {
            residue: 10,
            ..passed
        };
        let r = MappingReport::from_csw(&passed_short, 5, 512);
        assert!(r.short_transfer);
        assert_eq!(r.transferred, 502);
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(r.requires_sense());

        let failed = Csw {
            tag: 5,
            residue: 0,
            status: CswStatus::Failed,
        };
        let r = MappingReport::from_csw(&failed, 5, 512);
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(!r.requires_reset_recovery());

        let phase = Csw {
            tag: 5,
            residue: 0,
            status: CswStatus::PhaseError,
        };
        let r = MappingReport::from_csw(&phase, 5, 512);
        assert_eq!(r.outcome, Outcome::PhaseError);
        assert!(r.requires_reset_recovery());

        let wrong_tag = Csw {
            tag: 6,
            residue: 0,
            status: CswStatus::Passed,
        };
        let r = MappingReport::from_csw(&wrong_tag, 5, 512);
        assert!(!r.tag_matched);
        assert_eq!(r.outcome, Outcome::PhaseError);

        let overrun = Csw {
            tag: 5,
            residue: 600,
            status: CswStatus::Passed,
        };
        let r = MappingReport::from_csw(&overrun, 5, 512);
        assert!(r.overrun());
        assert_eq!(r.transferred, 0);
        assert_eq!(r.outcome, Outcome::PhaseError);
    }

    #[test]
    fn tag_generator_starts_at_one_and_wraps() {
        let mut g = TagGenerator::new(0);
        assert_eq!(g.next_tag(), 1);

        let mut g = TagGenerator::new(0xFFFF_FFFE);
        assert_eq!(g.next_tag(), 0xFFFF_FFFE);
        assert_eq!(g.next_tag(), 0xFFFF_FFFF);
        assert_eq!(g.next_tag(), 1, "wraps through zero");
        assert_eq!(g.next_tag(), 2);

        let mut g = TagGenerator::default();
        assert_eq!(g.next_tag(), 1);
    }

    #[test]
    fn plan_rejects_invalid_inputs() {
        assert_eq!(
            BotTransaction::new(1, 0, DataDirection::None, 0, &[]).unwrap_err(),
            PlanError::EmptyCdb
        );
        let long = [0u8; bot::MAX_CDB_LEN + 1];
        assert_eq!(
            BotTransaction::new(1, 0, DataDirection::In, 17, &long).unwrap_err(),
            PlanError::CdbTooLong
        );
        assert_eq!(
            BotTransaction::new(1, 16, DataDirection::None, 0, &[0]).unwrap_err(),
            PlanError::InvalidLun
        );
        assert_eq!(
            BotTransaction::new(1, 0, DataDirection::None, 4, &[0]).unwrap_err(),
            PlanError::LengthWithoutDirection
        );
    }
}
