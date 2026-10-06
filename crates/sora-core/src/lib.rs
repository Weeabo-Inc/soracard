//! # sora-core
//!
//! Transport-agnostic, allocation-free logic for the `SoraCard` driver.
//!
//! Everything in here is deterministic and side-effect free so it can be
//! exhaustively unit-tested on the host, far away from ring 0:
//!
//! * [`bot`]  — USB Mass Storage Bulk-Only Transport framing (CBW/CSW)
//! * [`bot_plan`] — the BOT phase state machine that drives those frames
//! * [`scsi`] — the SCSI command subset a card reader needs, plus parsers
//! * [`scsi_emu`] — commands answered locally, sense builder, RMB override
//! * [`scsi_target`] — answering SCSI ourselves (readers without SCSI firmware), media change
//! * [`card`] — card-presence detection policy (debounce / transition logic)
//! * [`rtsx`] — Realtek "RTCR" register protocol (RTS5129/5139 are not mass-storage)
//! * [`rtsx_sd`] — SD commands and block transfers as RTCR batches; clock maths
//! * [`sd`] — SD card protocol: commands, responses, CSD, init state machine
//!
//! The `StorPort` driver (`storport/`) is a thin shell over this.
//! [`card`] and [`quirks`] date from the earlier KMDF design (`sora-driver`)
//! and are not used by the `StorPort` driver yet.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(clippy::all)]
#![warn(clippy::pedantic)]

pub mod be;
pub mod bot;
pub mod bot_plan;
pub mod card;
pub mod quirks;
pub mod rtsx;
pub mod rtsx_sd;
pub mod scsi;
pub mod scsi_emu;
pub mod scsi_target;
pub mod sd;
