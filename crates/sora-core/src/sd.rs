//! SD card protocol (host side), per the SD Physical Layer Simplified
//! Specification.
//!
//! Controllers like the RTS5129 move bits on the SD bus but leave the protocol
//! to the host: which command to send, how to read the responses, and the
//! initialisation dance. This module is that logic, independent of any
//! controller, so it can be unit-tested on the host:
//!
//! * command framing and CRC7 ([`Command`], [`crc7`]);
//! * response decoding (R1 card status, R3 OCR, R6 RCA, R7 interface check);
//! * CSD → capacity ([`Csd::parse`]);
//! * the card initialisation state machine ([`Init`]): the driver asks it what
//!   to do next, performs it, and feeds the result back.
//!
//! Only SD memory cards are handled (SDSC v1/v2, SDHC, SDXC). MMC and SDIO are
//! rejected.

/// Response formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resp {
    None,
    /// 48-bit, card status.
    R1,
    /// R1 with busy signalling on DAT0.
    R1b,
    /// 136-bit, CID or CSD.
    R2,
    /// 48-bit, OCR, no valid CRC.
    R3,
    /// 48-bit, published RCA + status bits.
    R6,
    /// 48-bit, interface condition echo.
    R7,
}

impl Resp {
    /// Response length on the wire in bytes (including start/index/CRC).
    #[must_use]
    pub const fn wire_len(self) -> usize {
        match self {
            Self::None => 0,
            Self::R2 => 17,
            Self::R1 | Self::R1b | Self::R3 | Self::R6 | Self::R7 => 6,
        }
    }

    /// Whether the controller should verify the response CRC7.
    #[must_use]
    pub const fn has_crc(self) -> bool {
        !matches!(self, Self::None | Self::R3)
    }

    /// Whether the response carries a command-index field to check.
    #[must_use]
    pub const fn has_index(self) -> bool {
        !matches!(self, Self::None | Self::R2 | Self::R3)
    }
}

/// One SD bus command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub index: u8,
    pub arg: u32,
    pub resp: Resp,
    /// An application command: must be preceded by CMD55 (`APP_CMD`).
    pub app: bool,
}

impl Command {
    #[must_use]
    pub const fn new(index: u8, arg: u32, resp: Resp) -> Self {
        Self {
            index,
            arg,
            resp,
            app: false,
        }
    }

    #[must_use]
    pub const fn app(index: u8, arg: u32, resp: Resp) -> Self {
        Self {
            index,
            arg,
            resp,
            app: true,
        }
    }

    /// The 6-byte command token: start/transmission bits + index, argument
    /// (big-endian), CRC7 + end bit.
    #[must_use]
    pub fn frame(&self) -> [u8; 6] {
        let a = self.arg.to_be_bytes();
        let mut f = [0x40 | (self.index & 0x3F), a[0], a[1], a[2], a[3], 0];
        f[5] = (crc7(&f[..5]) << 1) | 1;
        f
    }
}

// Commands used by the driver.
#[must_use]
pub const fn go_idle() -> Command {
    Command::new(0, 0, Resp::None)
}
/// CMD8: 2.7–3.6 V, check pattern 0xAA.
#[must_use]
pub const fn send_if_cond() -> Command {
    Command::new(8, 0x1AA, Resp::R7)
}
#[must_use]
pub const fn app_cmd(rca: u16) -> Command {
    Command::new(55, (rca as u32) << 16, Resp::R1)
}
/// ACMD41. `hcs` asks for high-capacity support (SD v2 cards only); `s18r`
/// additionally requests 1.8 V signaling and maximum performance (XPC), which
/// UHS-I cards answer with S18A in the OCR.
#[must_use]
pub const fn sd_send_op_cond(hcs: bool, s18r: bool) -> Command {
    // 2.7–3.6 V window: 0x00FF8000.
    let mut arg = 0x00FF_8000;
    if hcs {
        arg |= 0x4000_0000;
    }
    if hcs && s18r {
        arg |= 0x1100_0000; // XPC (bit 28) | S18R (bit 24)
    }
    Command::app(41, arg, Resp::R3)
}
/// CMD11: switch the card's signaling to 1.8 V (UHS-I).
#[must_use]
pub const fn voltage_switch() -> Command {
    Command::new(11, 0, Resp::R1)
}
/// CMD19: send the 64-byte tuning block (UHS-I SDR50/SDR104).
#[must_use]
pub const fn send_tuning_block() -> Command {
    Command::new(19, 0, Resp::R1)
}
/// Length of the CMD19 tuning block (4-bit bus).
pub const TUNING_BLOCK_LEN: u16 = 64;
#[must_use]
pub const fn all_send_cid() -> Command {
    Command::new(2, 0, Resp::R2)
}
#[must_use]
pub const fn send_relative_addr() -> Command {
    Command::new(3, 0, Resp::R6)
}
#[must_use]
pub const fn send_csd(rca: u16) -> Command {
    Command::new(9, (rca as u32) << 16, Resp::R2)
}
#[must_use]
pub const fn select_card(rca: u16) -> Command {
    Command::new(7, (rca as u32) << 16, Resp::R1b)
}
/// ACMD6: 4-bit bus.
#[must_use]
pub const fn set_bus_width_4() -> Command {
    Command::app(6, 2, Resp::R1)
}
#[must_use]
pub const fn set_blocklen_512() -> Command {
    Command::new(16, 512, Resp::R1)
}
#[must_use]
pub const fn send_status(rca: u16) -> Command {
    Command::new(13, (rca as u32) << 16, Resp::R1)
}
/// CMD6 (`SWITCH_FUNC`) selecting `function` in function `group` (1–6),
/// leaving every other group unchanged (0xF). `set = false` only asks (mode
/// 0), `set = true` switches (mode 1). The card returns a 64-byte status
/// block on the data lines.
#[must_use]
pub const fn switch_function(set: bool, group: u8, function: u8) -> Command {
    let shift = 4 * ((group.saturating_sub(1) as u32) % 6);
    let arg = (0x00FF_FFFF & !(0xF << shift)) | (((function & 0xF) as u32) << shift);
    Command::new(6, if set { 0x8000_0000 | arg } else { arg }, Resp::R1)
}

/// Function group 1 (bus speed mode).
pub const GROUP_BUS_SPEED: u8 = 1;
/// Function group 4 (current limit).
pub const GROUP_CURRENT_LIMIT: u8 = 4;
/// Group 1: High Speed / SDR25 (50 MHz).
pub const FN_HIGH_SPEED: u8 = 1;
/// Group 1: SDR50 (100 MHz, 1.8 V).
pub const FN_SDR50: u8 = 2;

/// CMD6 for High Speed (group 1, function 1).
#[must_use]
pub const fn switch_high_speed(set: bool) -> Command {
    switch_function(set, GROUP_BUS_SPEED, FN_HIGH_SPEED)
}

/// Whether a CMD6 status block (512 bits, MSB first) lists `function` as
/// supported in `group`: group g's 16 support bits start at bit
/// 400 + 16·(g−1).
#[must_use]
pub fn switch_supports(status: &[u8], group: u8, function: u8) -> bool {
    if !(1..=6).contains(&group) || function > 15 {
        return false;
    }
    let bit = 400 + 16 * (usize::from(group) - 1) + usize::from(function);
    status
        .get((511 - bit) / 8)
        .is_some_and(|b| b >> (bit % 8) & 1 != 0)
}

/// The function a CMD6 status block reports for `group` (0xF = error or
/// unsupported): group g's 4-bit field starts at bit 376 + 4·(g−1).
#[must_use]
pub fn switch_selected(status: &[u8], group: u8) -> u8 {
    if !(1..=6).contains(&group) {
        return 0xF;
    }
    let low = 376 + 4 * (usize::from(group) - 1);
    status
        .get((511 - low) / 8)
        .map_or(0xF, |b| (b >> (low % 8)) & 0xF)
}

/// Length of the CMD6 switch-function status block.
pub const SWITCH_STATUS_LEN: usize = 64;

/// Whether a CMD6 status block says function group 1 supports High Speed.
#[must_use]
pub fn switch_supports_high_speed(status: &[u8]) -> bool {
    switch_supports(status, GROUP_BUS_SPEED, FN_HIGH_SPEED)
}

/// Whether a CMD6 status block reports High Speed selected in group 1.
#[must_use]
pub fn switch_selected_high_speed(status: &[u8]) -> bool {
    switch_selected(status, GROUP_BUS_SPEED) == FN_HIGH_SPEED
}

#[must_use]
pub const fn stop_transmission() -> Command {
    Command::new(12, 0, Resp::R1b)
}
#[must_use]
pub const fn read_single(addr: u32) -> Command {
    Command::new(17, addr, Resp::R1)
}
#[must_use]
pub const fn read_multiple(addr: u32) -> Command {
    Command::new(18, addr, Resp::R1)
}
#[must_use]
pub const fn write_single(addr: u32) -> Command {
    Command::new(24, addr, Resp::R1)
}
#[must_use]
pub const fn write_multiple(addr: u32) -> Command {
    Command::new(25, addr, Resp::R1)
}

/// The data-command address for `lba`: block number for high-capacity cards,
/// byte offset for standard-capacity ones. `None` if it does not fit.
#[must_use]
pub fn block_address(lba: u64, high_capacity: bool) -> Option<u32> {
    let a = if high_capacity {
        lba
    } else {
        lba.checked_mul(512)?
    };
    u32::try_from(a).ok()
}

/// CRC7 (x^7 + x^3 + 1) over `bytes`, MSB first, as used on the CMD line.
#[must_use]
pub fn crc7(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        for i in (0..8).rev() {
            let bit = (b >> i) & 1;
            let top = (crc >> 6) & 1;
            crc = (crc << 1) & 0x7F;
            if bit ^ top != 0 {
                crc ^= 0x09;
            }
        }
    }
    crc
}

/// R1 card-status error bits (any set → command failed).
pub const R1_ERRORS: u32 = 0xFDF9_0008;
/// R1 `CURRENT_STATE` field.
#[must_use]
pub const fn r1_state(status: u32) -> u8 {
    ((status >> 9) & 0xF) as u8
}
pub const STATE_TRAN: u8 = 4;

/// The 32-bit payload of a 48-bit response (bytes 1..5 of the 6-byte token).
#[must_use]
pub fn payload48(r: &[u8]) -> Option<u32> {
    if r.len() < 5 {
        return None;
    }
    Some(u32::from_be_bytes([r[1], r[2], r[3], r[4]]))
}

/// OCR from an R3 response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ocr(pub u32);
impl Ocr {
    /// Power-up finished (bit 31 set = not busy).
    #[must_use]
    pub const fn ready(self) -> bool {
        self.0 & 0x8000_0000 != 0
    }
    /// Card Capacity Status: high capacity (SDHC/SDXC).
    #[must_use]
    pub const fn high_capacity(self) -> bool {
        self.0 & 0x4000_0000 != 0
    }
    /// S18A: the card accepts switching to 1.8 V signaling (UHS-I).
    #[must_use]
    pub const fn s18a(self) -> bool {
        self.0 & 0x0100_0000 != 0
    }
}

/// Card-specific data, parsed for what the driver needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csd {
    pub version: u8,
    /// Capacity in 512-byte blocks.
    pub blocks: u64,
    /// Card is permanently or temporarily write protected.
    pub write_protected: bool,
}

impl Csd {
    /// Parse the 16 CSD bytes (bits 127..0, i.e. an R2 payload without the
    /// leading header byte).
    #[must_use]
    pub fn parse(c: &[u8]) -> Option<Self> {
        if c.len() < 16 {
            return None;
        }
        // bits(hi, lo): extract CSD bits hi..=lo (bit 127 is c[0] bit 7).
        let bits = |hi: u32, lo: u32| -> u64 {
            let mut v = 0u64;
            for bit in (lo..=hi).rev() {
                let byte = c[(15 - bit / 8) as usize];
                v = (v << 1) | u64::from((byte >> (bit % 8)) & 1);
            }
            v
        };
        // CSD_STRUCTURE is a 2-bit field, so it always fits.
        let version = u8::try_from(bits(127, 126)).unwrap_or(u8::MAX);
        let blocks = match version {
            0 => {
                let read_bl_len = bits(83, 80);
                let c_size = bits(73, 62);
                let c_size_mult = bits(49, 47);
                let bytes = (c_size + 1) << (c_size_mult + 2 + read_bl_len);
                bytes / 512
            }
            1 => (bits(69, 48) + 1) * 1024,
            _ => return None,
        };
        let write_protected = bits(13, 13) == 1 || bits(12, 12) == 1;
        Some(Self {
            version,
            blocks,
            write_protected,
        })
    }
}

/// What the driver must do next during initialisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Send this command (with CMD55 first if `app`) and report the result.
    Send(Command),
    /// Wait this many milliseconds, then call [`Init::next`] with `Outcome::Waited`.
    Wait(u32),
    /// CMD11 was accepted: switch the host to 1.8 V signaling now (pads,
    /// clock gating, DAT-line checks), then call [`Init::next`] with
    /// `Outcome::Waited`. If the host switch fails, abandon this init,
    /// power-cycle the card and start again without 1.8 V.
    SwitchVoltage,
    /// Card is ready for data transfer.
    Ready(CardInfo),
    /// Give up.
    Fail(InitError),
}

/// Result of performing a [`Step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome<'a> {
    /// The command completed; response token (6 or 17 bytes; empty for none).
    Response(&'a [u8]),
    /// No response (timeout) or CRC error.
    NoResponse,
    Waited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// No answer to any command: no card, or not an SD memory card.
    NoCard,
    /// CMD8 answered with the wrong echo: unusable card.
    BadInterfaceCondition,
    /// ACMD41 never finished power-up.
    PowerUpTimeout,
    /// A command failed in a way the sequence cannot recover from.
    Protocol(u8),
    /// The switch to 1.8 V signaling failed (power-cycle, retry without it).
    VoltageSwitch,
}

/// What initialisation learned about the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardInfo {
    pub rca: u16,
    pub high_capacity: bool,
    pub blocks: u64,
    pub write_protected: bool,
    pub cid: [u8; 16],
    /// The card switched to 1.8 V signaling (UHS-I); it can run SDR50.
    pub uhs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Idle,
    IfCond,
    OpCond,
    OpCondWait,
    VoltageSwitch,
    VoltageSwitched,
    Cid,
    Rca,
    Csd,
    Select,
    BusWidth,
    BlockLen,
}

/// ACMD41 polling: up to ~1 s (SD spec allows 1 s for power-up).
const OPCOND_TRIES: u16 = 100;
const OPCOND_WAIT_MS: u32 = 10;

/// The card initialisation state machine (identification → transfer state,
/// 4-bit bus, 512-byte blocks).
#[derive(Debug, Clone, Copy)]
pub struct Init {
    state: State,
    v2: bool,
    /// Request 1.8 V signaling (UHS-I) from cards that support it.
    want_uhs: bool,
    tries: u16,
    info: CardInfo,
}

impl Default for Init {
    fn default() -> Self {
        Self::new()
    }
}

impl Init {
    #[must_use]
    pub const fn new() -> Self {
        Self::with_uhs(false)
    }

    /// Like [`Init::new`], but with `want_uhs` the card is asked for 1.8 V
    /// signaling; a UHS-I card then goes through CMD11 and
    /// [`Step::SwitchVoltage`], and comes out with [`CardInfo::uhs`] set.
    #[must_use]
    pub const fn with_uhs(want_uhs: bool) -> Self {
        Self {
            state: State::Start,
            v2: false,
            want_uhs,
            tries: 0,
            info: CardInfo {
                rca: 0,
                high_capacity: false,
                blocks: 0,
                write_protected: false,
                cid: [0; 16],
                uhs: false,
            },
        }
    }

    /// Advance with the outcome of the previous step (`Outcome::Waited` for
    /// the very first call).
    pub fn next(&mut self, outcome: Outcome<'_>) -> Step {
        use Outcome::{NoResponse, Response, Waited};
        match (self.state, outcome) {
            (State::Start, _) => {
                self.state = State::Idle;
                Step::Send(go_idle())
            }
            (State::Idle, _) => {
                // CMD0 has no response; move on to the interface check.
                self.state = State::IfCond;
                Step::Send(send_if_cond())
            }
            (State::IfCond, Response(r)) => {
                // R7: voltage accepted (bits 11:8 = 1) and echo 0xAA.
                match payload48(r) {
                    Some(p) if p & 0xFFF == 0x1AA => {
                        self.v2 = true;
                        self.op_cond()
                    }
                    _ => Step::Fail(InitError::BadInterfaceCondition),
                }
            }
            (State::IfCond, NoResponse) => {
                // SD v1.x card (or MMC): no CMD8.
                self.v2 = false;
                self.op_cond()
            }
            (State::OpCond, Response(r)) => {
                let ocr = Ocr(payload48(r).unwrap_or(0));
                if ocr.ready() {
                    self.info.high_capacity = self.v2 && ocr.high_capacity();
                    if self.want_uhs && self.info.high_capacity && ocr.s18a() {
                        self.state = State::VoltageSwitch;
                        return Step::Send(voltage_switch());
                    }
                    self.state = State::Cid;
                    Step::Send(all_send_cid())
                } else if self.tries >= OPCOND_TRIES {
                    Step::Fail(InitError::PowerUpTimeout)
                } else {
                    self.state = State::OpCondWait;
                    Step::Wait(OPCOND_WAIT_MS)
                }
            }
            (State::OpCond, NoResponse) => Step::Fail(InitError::NoCard),
            (State::OpCondWait, Waited) => self.op_cond(),
            (State::VoltageSwitch, Response(r)) => self.checked(r, 11, |s| {
                s.state = State::VoltageSwitched;
                Step::SwitchVoltage
            }),
            (State::VoltageSwitch, NoResponse) => Step::Fail(InitError::VoltageSwitch),
            (State::VoltageSwitched, Waited) => {
                self.info.uhs = true;
                self.state = State::Cid;
                Step::Send(all_send_cid())
            }
            (State::Cid, Response(r)) if r.len() >= 17 => {
                self.info.cid.copy_from_slice(&r[1..17]);
                self.state = State::Rca;
                Step::Send(send_relative_addr())
            }
            (State::Rca, Response(r)) => {
                let p = payload48(r).unwrap_or(0);
                #[allow(clippy::cast_possible_truncation)]
                let rca = (p >> 16) as u16;
                if rca == 0 {
                    // RCA 0 is reserved; ask again (spec allows re-issuing CMD3).
                    return Step::Send(send_relative_addr());
                }
                self.info.rca = rca;
                self.state = State::Csd;
                Step::Send(send_csd(rca))
            }
            (State::Csd, Response(r)) if r.len() >= 17 => match Csd::parse(&r[1..17]) {
                Some(csd) => {
                    self.info.blocks = csd.blocks;
                    self.info.write_protected = csd.write_protected;
                    self.state = State::Select;
                    Step::Send(select_card(self.info.rca))
                }
                None => Step::Fail(InitError::Protocol(9)),
            },
            (State::Select, Response(r)) => self.checked(r, 7, |s| {
                s.state = State::BusWidth;
                Step::Send(set_bus_width_4())
            }),
            (State::BusWidth, Response(r)) => self.checked(r, 6, |s| {
                if s.info.high_capacity {
                    Step::Ready(s.info)
                } else {
                    s.state = State::BlockLen;
                    Step::Send(set_blocklen_512())
                }
            }),
            (State::BlockLen, Response(r)) => self.checked(r, 16, |s| Step::Ready(s.info)),
            (state, _) => Step::Fail(InitError::Protocol(state_cmd(state))),
        }
    }

    /// The card's relative address once CMD3 has completed (0 before), for
    /// the CMD55 that precedes application commands.
    #[must_use]
    pub const fn rca(&self) -> u16 {
        self.info.rca
    }

    fn op_cond(&mut self) -> Step {
        self.tries += 1;
        self.state = State::OpCond;
        Step::Send(sd_send_op_cond(self.v2, self.want_uhs))
    }

    fn checked(&mut self, r: &[u8], cmd: u8, ok: impl FnOnce(&mut Self) -> Step) -> Step {
        match payload48(r) {
            Some(status) if status & R1_ERRORS == 0 => ok(self),
            _ => Step::Fail(InitError::Protocol(cmd)),
        }
    }
}

const fn state_cmd(s: State) -> u8 {
    match s {
        State::Start | State::Idle => 0,
        State::IfCond => 8,
        State::OpCond | State::OpCondWait => 41,
        State::VoltageSwitch | State::VoltageSwitched => 11,
        State::Cid => 2,
        State::Rca => 3,
        State::Csd => 9,
        State::Select => 7,
        State::BusWidth => 6,
        State::BlockLen => 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc7_known_values() {
        // Well-known command tokens: CMD0 → 0x95, CMD8(0x1AA) → 0x87.
        assert_eq!(go_idle().frame(), [0x40, 0, 0, 0, 0, 0x95]);
        assert_eq!(send_if_cond().frame(), [0x48, 0, 0, 0x01, 0xAA, 0x87]);
    }

    #[test]
    fn addressing() {
        assert_eq!(block_address(10, true), Some(10));
        assert_eq!(block_address(10, false), Some(5120));
        assert_eq!(block_address(1 << 23, false), None); // 4 GiB byte offset overflows
    }

    fn r48(index: u8, payload: u32) -> [u8; 6] {
        let p = payload.to_be_bytes();
        [index, p[0], p[1], p[2], p[3], 1]
    }

    /// Build a CSD v2 with the given `C_SIZE` (capacity = (`C_SIZE+1`) * 512 KiB).
    fn csd_v2(c_size: u32) -> [u8; 16] {
        let mut c = [0u8; 16];
        c[0] = 0x40; // CSD_STRUCTURE = 1
                     // C_SIZE is bits 69..48: byte 7 low 6 bits, byte 8, byte 9.
        let b = c_size.to_be_bytes();
        c[7] = b[1] & 0x3F;
        c[8] = b[2];
        c[9] = b[3];
        c
    }

    #[test]
    fn csd_v2_capacity() {
        // 128 GB-class card: C_SIZE 238_590 → 244_310_016 blocks.
        let csd = Csd::parse(&csd_v2(238_590)).unwrap();
        assert_eq!(csd.version, 1);
        assert_eq!(csd.blocks, (238_590 + 1) * 1024);
        assert!(!csd.write_protected);
    }

    #[test]
    fn csd_v1_capacity() {
        // 1 GB SDSC: READ_BL_LEN 10 (1024), C_SIZE 3863, C_SIZE_MULT 6.
        let mut c = [0u8; 16];
        c[5] = 0x0A; // READ_BL_LEN in bits 83..80
                     // C_SIZE bits 73..62 = 3863 (0xF17): byte 6 bits 1..0, byte 7, byte 8 bits 7..6.
        c[6] = 0x03;
        c[7] = 0xC5;
        c[8] = 0xC0;
        // C_SIZE_MULT bits 49..47 = 6: byte 9 bits 1..0 = 0b11, byte 10 bit 7 = 0.
        c[9] = 0x03;
        let csd = Csd::parse(&c).unwrap();
        assert_eq!(csd.version, 0);
        assert_eq!(csd.blocks, ((3863 + 1) << (6 + 2 + 10)) / 512);
    }

    #[test]
    fn init_sdhc_happy_path() {
        let mut i = Init::new();
        assert_eq!(i.next(Outcome::Waited), Step::Send(go_idle()));
        assert_eq!(i.next(Outcome::Response(&[])), Step::Send(send_if_cond()));
        assert_eq!(
            i.next(Outcome::Response(&r48(8, 0x1AA))),
            Step::Send(sd_send_op_cond(true, false))
        );
        // Busy once, then ready with CCS.
        assert_eq!(
            i.next(Outcome::Response(&r48(0x3F, 0x00FF_8000))),
            Step::Wait(10)
        );
        assert_eq!(
            i.next(Outcome::Waited),
            Step::Send(sd_send_op_cond(true, false))
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(0x3F, 0xC0FF_8000))),
            Step::Send(all_send_cid())
        );
        let mut cid = [0u8; 17];
        cid[1] = 0x03;
        assert_eq!(
            i.next(Outcome::Response(&cid)),
            Step::Send(send_relative_addr())
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(3, 0xB368_0500))),
            Step::Send(send_csd(0xB368))
        );
        let mut csd = [0x3Fu8; 17];
        csd[1..].copy_from_slice(&csd_v2(238_590));
        assert_eq!(
            i.next(Outcome::Response(&csd)),
            Step::Send(select_card(0xB368))
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(7, 0x0700))),
            Step::Send(set_bus_width_4())
        );
        match i.next(Outcome::Response(&r48(6, 0x0920))) {
            Step::Ready(info) => {
                assert_eq!(info.rca, 0xB368);
                assert!(info.high_capacity);
                assert_eq!(info.blocks, 238_591 * 1024);
                assert_eq!(info.cid[0], 0x03);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn init_sdsc_v1_sets_blocklen() {
        let mut i = Init::new();
        i.next(Outcome::Waited);
        i.next(Outcome::Response(&[]));
        assert_eq!(
            i.next(Outcome::NoResponse),
            Step::Send(sd_send_op_cond(false, false))
        );
        // v1 cards never report high capacity even if the bit were set.
        i.next(Outcome::Response(&r48(0x3F, 0xC0FF_8000)));
        i.next(Outcome::Response(&[0u8; 17]));
        i.next(Outcome::Response(&r48(3, 0x1234_0000)));
        let mut csd = [0u8; 17];
        csd[1..].copy_from_slice(&csd_v2(1)); // structure doesn't matter here
        i.next(Outcome::Response(&csd));
        i.next(Outcome::Response(&r48(7, 0)));
        assert_eq!(
            i.next(Outcome::Response(&r48(6, 0))),
            Step::Send(set_blocklen_512())
        );
        assert!(
            matches!(i.next(Outcome::Response(&r48(16, 0))), Step::Ready(info) if !info.high_capacity)
        );
    }

    #[test]
    fn init_failures() {
        // Wrong CMD8 echo.
        let mut i = Init::new();
        i.next(Outcome::Waited);
        i.next(Outcome::Response(&[]));
        assert_eq!(
            i.next(Outcome::Response(&r48(8, 0x155))),
            Step::Fail(InitError::BadInterfaceCondition)
        );
        // No answer to ACMD41.
        let mut i = Init::new();
        i.next(Outcome::Waited);
        i.next(Outcome::Response(&[]));
        i.next(Outcome::NoResponse);
        assert_eq!(i.next(Outcome::NoResponse), Step::Fail(InitError::NoCard));
        // Power-up never completes.
        let mut i = Init::new();
        i.next(Outcome::Waited);
        i.next(Outcome::Response(&[]));
        i.next(Outcome::Response(&r48(8, 0x1AA)));
        let mut last = Step::Wait(0);
        for _ in 0..200 {
            last = i.next(Outcome::Response(&r48(0x3F, 0x00FF_8000)));
            if matches!(last, Step::Fail(_)) {
                break;
            }
            i.next(Outcome::Waited);
        }
        assert_eq!(last, Step::Fail(InitError::PowerUpTimeout));
        // Error bit in R1 after CMD7.
        let mut i = Init::new();
        i.next(Outcome::Waited);
        i.next(Outcome::Response(&[]));
        i.next(Outcome::Response(&r48(8, 0x1AA)));
        i.next(Outcome::Response(&r48(0x3F, 0xC0FF_8000)));
        i.next(Outcome::Response(&[0u8; 17]));
        i.next(Outcome::Response(&r48(3, 0x0001_0000)));
        let mut csd = [0u8; 17];
        csd[1..].copy_from_slice(&csd_v2(10));
        i.next(Outcome::Response(&csd));
        assert_eq!(
            i.next(Outcome::Response(&r48(7, 0x8000_0000))),
            Step::Fail(InitError::Protocol(7))
        );
    }

    #[test]
    fn r1_helpers() {
        assert_eq!(r1_state(0x0900), STATE_TRAN);
        assert_eq!(0x0900 & R1_ERRORS, 0);
        assert_ne!(0x8000_0000 & R1_ERRORS, 0); // OUT_OF_RANGE
    }
    #[test]
    fn switch_high_speed_commands() {
        assert_eq!(switch_high_speed(false).arg, 0x00FF_FFF1);
        assert_eq!(switch_high_speed(true).arg, 0x80FF_FFF1);
        assert_eq!(switch_high_speed(true).index, 6);
    }

    #[test]
    fn switch_status_parsing() {
        let mut st = [0u8; SWITCH_STATUS_LEN];
        assert!(!switch_supports_high_speed(&st));
        assert!(!switch_selected_high_speed(&st));
        st[13] = 0x03; // functions 0 and 1 supported in group 1
        st[16] = 0x01; // group 1 switched to function 1
        assert!(switch_supports_high_speed(&st));
        assert!(switch_selected_high_speed(&st));
        st[16] = 0x0F; // 0xF: the switch failed
        assert!(!switch_selected_high_speed(&st));
        assert!(!switch_supports_high_speed(&st[..10]));
    }
    /// Drive an init through CMD2..ACMD6 from the CID step to `Ready`.
    fn finish_from_cid(i: &mut Init) -> CardInfo {
        let mut cid = [0u8; 17];
        cid[1] = 0x03;
        assert_eq!(
            i.next(Outcome::Response(&cid)),
            Step::Send(send_relative_addr())
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(3, 0xB368_0500))),
            Step::Send(send_csd(0xB368))
        );
        let mut csd = [0x3Fu8; 17];
        csd[1..].copy_from_slice(&csd_v2(238_590));
        assert_eq!(
            i.next(Outcome::Response(&csd)),
            Step::Send(select_card(0xB368))
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(7, 0x0700))),
            Step::Send(set_bus_width_4())
        );
        match i.next(Outcome::Response(&r48(6, 0x0920))) {
            Step::Ready(info) => info,
            other => panic!("{other:?}"),
        }
    }

    /// Start an init and answer CMD0/CMD8, returning the first ACMD41.
    fn to_op_cond(i: &mut Init) -> Step {
        assert_eq!(i.next(Outcome::Waited), Step::Send(go_idle()));
        assert_eq!(i.next(Outcome::Response(&[])), Step::Send(send_if_cond()));
        i.next(Outcome::Response(&r48(8, 0x1AA)))
    }

    #[test]
    fn op_cond_requests_1v8_only_when_asked() {
        assert_eq!(sd_send_op_cond(true, false).arg, 0x40FF_8000);
        assert_eq!(sd_send_op_cond(true, true).arg, 0x51FF_8000);
        // Never for v1 cards (no HCS).
        assert_eq!(sd_send_op_cond(false, true).arg, 0x00FF_8000);
    }

    #[test]
    fn init_uhs_switches_voltage() {
        let mut i = Init::with_uhs(true);
        assert_eq!(to_op_cond(&mut i), Step::Send(sd_send_op_cond(true, true)));
        // Ready, CCS and S18A.
        assert_eq!(
            i.next(Outcome::Response(&r48(0x3F, 0xC1FF_8000))),
            Step::Send(voltage_switch())
        );
        assert_eq!(
            i.next(Outcome::Response(&r48(11, 0x0000_0000))),
            Step::SwitchVoltage
        );
        assert_eq!(i.next(Outcome::Waited), Step::Send(all_send_cid()));
        let info = finish_from_cid(&mut i);
        assert!(info.uhs);
        assert!(info.high_capacity);
    }

    #[test]
    fn init_uhs_card_without_s18a_stays_3v3() {
        let mut i = Init::with_uhs(true);
        let _ = to_op_cond(&mut i);
        assert_eq!(
            i.next(Outcome::Response(&r48(0x3F, 0xC0FF_8000))),
            Step::Send(all_send_cid())
        );
        assert!(!finish_from_cid(&mut i).uhs);
    }

    #[test]
    fn init_uhs_cmd11_failure() {
        let mut i = Init::with_uhs(true);
        let _ = to_op_cond(&mut i);
        let _ = i.next(Outcome::Response(&r48(0x3F, 0xC1FF_8000)));
        assert_eq!(
            i.next(Outcome::NoResponse),
            Step::Fail(InitError::VoltageSwitch)
        );
    }

    #[test]
    fn s18a_ignored_when_not_asked() {
        let mut i = Init::new();
        let _ = to_op_cond(&mut i);
        assert_eq!(
            i.next(Outcome::Response(&r48(0x3F, 0xC1FF_8000))),
            Step::Send(all_send_cid())
        );
    }

    #[test]
    fn switch_function_encoding() {
        assert_eq!(switch_function(false, 1, 1).arg, 0x00FF_FFF1);
        assert_eq!(switch_function(true, 1, 2).arg, 0x80FF_FFF2);
        assert_eq!(switch_function(true, 4, 3).arg, 0x80FF_3FFF);
        assert_eq!(switch_function(true, 4, 0).arg, 0x80FF_0FFF);
    }

    #[test]
    fn switch_status_groups() {
        let mut st = [0u8; SWITCH_STATUS_LEN];
        // Group 1 supports functions 0, 1, 2 (bits 400..402).
        st[13] = 0x07;
        // Group 4 supports functions 0..3 (bits 448..451 -> byte 7).
        st[7] = 0x0F;
        // Selected: group 1 = 2 (byte 16 low nibble), group 4 = 3 (byte 15 high nibble).
        st[16] = 0x02;
        st[15] = 0x30;
        assert!(switch_supports(&st, GROUP_BUS_SPEED, FN_SDR50));
        assert!(!switch_supports(&st, GROUP_BUS_SPEED, 3));
        assert!(switch_supports(&st, GROUP_CURRENT_LIMIT, 3));
        assert!(!switch_supports(&st, GROUP_CURRENT_LIMIT, 4));
        assert_eq!(switch_selected(&st, GROUP_BUS_SPEED), FN_SDR50);
        assert_eq!(switch_selected(&st, GROUP_CURRENT_LIMIT), 3);
        assert_eq!(switch_selected(&st, 7), 0xF);
        assert!(!switch_supports(&st, 0, 0));
    }
}
