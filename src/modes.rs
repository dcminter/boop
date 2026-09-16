const ESC: u8 = 0x1b;
const MAX_PARAMS: usize = 16;

const TRACKED: [(u16, bool); 14] = [
    (1049, false),
    (1047, false),
    (47, false),
    (1, false),
    (7, true),
    (25, true),
    (1000, false),
    (1002, false),
    (1003, false),
    (1004, false),
    (1005, false),
    (1006, false),
    (1015, false),
    (2004, false),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    ControlString,
}

#[derive(Debug)]
pub struct ModeTracker {
    state: State,
    leader: Option<u8>,
    params: Vec<u16>,
    current: Option<u16>,
    ignore: bool,
    parameter_seen: bool,
    modes: [bool; TRACKED.len()],
    keypad: bool,
}

impl Default for ModeTracker {
    fn default() -> Self {
        ModeTracker {
            state: State::Ground,
            leader: None,
            params: Vec::new(),
            current: None,
            ignore: false,
            parameter_seen: false,
            modes: TRACKED.map(|(_, default)| default),
            keypad: false,
        }
    }
}

impl ModeTracker {
    pub fn scan(&mut self, data: &[u8]) {
        for &byte in data {
            self.step(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        match byte {
            ESC => {
                self.state = State::Escape;
                return;
            }
            0x18 | 0x1a => {
                self.state = State::Ground;
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground => {}
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Csi => self.csi(byte),
            State::ControlString => {
                if byte == 0x07 {
                    self.state = State::Ground;
                }
            }
        }
    }

    fn escape(&mut self, byte: u8) {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.state = State::Csi;
                self.leader = None;
                self.params.clear();
                self.current = None;
                self.ignore = false;
                self.parameter_seen = false;
            }
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::ControlString,
            b'=' => self.keypad = true,
            b'>' => self.keypad = false,
            b'c' => *self = ModeTracker::default(),
            0x20..=0x2f => self.state = State::EscapeIntermediate,
            _ => {}
        }
    }

    fn csi(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                self.parameter_seen = true;
                let digit = u16::from(byte - b'0');
                let value = self.current.unwrap_or(0);
                self.current = Some(value.saturating_mul(10).saturating_add(digit));
            }
            b';' => {
                self.parameter_seen = true;
                self.push_param();
            }
            b':' => self.ignore = true,
            0x3c..=0x3f => {
                if self.parameter_seen || self.leader.is_some() {
                    self.ignore = true;
                } else {
                    self.leader = Some(byte);
                }
            }
            0x20..=0x2f => self.ignore = true,
            0x40..=0x7e => {
                self.state = State::Ground;
                self.push_param();
                if !self.ignore && self.leader == Some(b'?') {
                    match byte {
                        b'h' => self.apply(true),
                        b'l' => self.apply(false),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn push_param(&mut self) {
        if self.params.len() >= MAX_PARAMS {
            self.ignore = true;
        } else if let Some(value) = self.current.take() {
            self.params.push(value);
        }
    }

    fn apply(&mut self, enabled: bool) {
        for param in &self.params {
            if let Some(index) = TRACKED.iter().position(|(mode, _)| mode == param) {
                self.modes[index] = enabled;
            }
        }
    }

    fn differing(&self) -> impl Iterator<Item = (u16, bool)> + '_ {
        TRACKED
            .iter()
            .zip(self.modes)
            .filter(|((_, default), current)| default != current)
            .map(|((mode, _), current)| (*mode, current))
    }

    pub fn replay(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (mode, enabled) in self.differing() {
            out.extend(set_mode(mode, enabled));
        }
        if self.keypad {
            out.extend(b"\x1b=");
        }
        out
    }

    pub fn reset(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (mode, enabled) in self.differing() {
            out.extend(set_mode(mode, !enabled));
        }
        if self.keypad {
            out.extend(b"\x1b>");
        }
        out.extend(b"\x1b[0m");
        out
    }
}

fn set_mode(mode: u16, enabled: bool) -> Vec<u8> {
    format!("\x1b[?{mode}{}", if enabled { 'h' } else { 'l' }).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(data: &[u8]) -> ModeTracker {
        let mut tracker = ModeTracker::default();
        tracker.scan(data);
        tracker
    }

    fn replay(data: &[u8]) -> String {
        String::from_utf8(scanned(data).replay()).unwrap()
    }

    #[test]
    fn defaults_replay_nothing() {
        assert_eq!(replay(b""), "");
        assert_eq!(scanned(b"").reset(), b"\x1b[0m");
    }

    #[test]
    fn single_mode() {
        assert_eq!(replay(b"\x1b[?1000h"), "\x1b[?1000h");
        assert_eq!(replay(b"\x1b[?1000h\x1b[?1000l"), "");
    }

    #[test]
    fn default_on_modes() {
        assert_eq!(replay(b"\x1b[?25l"), "\x1b[?25l");
        assert_eq!(replay(b"\x1b[?25l\x1b[?25h"), "");
        assert_eq!(
            String::from_utf8(scanned(b"\x1b[?25l").reset()).unwrap(),
            "\x1b[?25h\x1b[0m"
        );
    }

    #[test]
    fn multiple_parameters() {
        assert_eq!(replay(b"\x1b[?1002;1006;9999h"), "\x1b[?1002h\x1b[?1006h");
    }

    #[test]
    fn alternate_screen_first() {
        let tracker = scanned(b"\x1b[?2004h\x1b[?1049h");
        assert_eq!(
            String::from_utf8(tracker.replay()).unwrap(),
            "\x1b[?1049h\x1b[?2004h"
        );
        assert_eq!(
            String::from_utf8(tracker.reset()).unwrap(),
            "\x1b[?1049l\x1b[?2004l\x1b[0m"
        );
    }

    #[test]
    fn split_across_chunks() {
        let data = b"text\x1b[?1049h more \x1b[?1000;1006h tail";
        for split in 0..data.len() {
            let mut tracker = ModeTracker::default();
            tracker.scan(&data[..split]);
            tracker.scan(&data[split..]);
            assert_eq!(
                String::from_utf8(tracker.replay()).unwrap(),
                "\x1b[?1049h\x1b[?1000h\x1b[?1006h",
                "split at {split}"
            );
        }
    }

    #[test]
    fn non_private_ignored() {
        assert_eq!(replay(b"\x1b[1000h\x1b[4h\x1b[>1000h\x1b[=25l"), "");
    }

    #[test]
    fn other_finals_ignored() {
        assert_eq!(replay(b"\x1b[?1000$p\x1b[?25s\x1b[?1049m"), "");
    }

    #[test]
    fn misplaced_leader_ignored() {
        assert_eq!(replay(b"\x1b[1?1000h\x1b[??1000h"), "");
    }

    #[test]
    fn colon_parameters_ignored() {
        assert_eq!(replay(b"\x1b[?1000:1h"), "");
    }

    #[test]
    fn keypad() {
        assert_eq!(replay(b"\x1b="), "\x1b=");
        assert_eq!(replay(b"\x1b=\x1b>"), "");
        assert_eq!(
            String::from_utf8(scanned(b"\x1b=").reset()).unwrap(),
            "\x1b>\x1b[0m"
        );
    }

    #[test]
    fn full_reset() {
        assert_eq!(replay(b"\x1b[?1049h\x1b=\x1b[?25l\x1bc"), "");
    }

    #[test]
    fn control_strings_hide_sequences() {
        assert_eq!(replay(b"\x1b]0;[?1000h\x07"), "");
        assert_eq!(replay(b"\x1bPq=\x1b\\"), "");
        assert_eq!(replay(b"\x1b]2;title\x1b\\\x1b[?1000h"), "\x1b[?1000h");
        assert_eq!(replay(b"\x1b]2;title\x07\x1b="), "\x1b=");
    }

    #[test]
    fn escape_intermediates() {
        assert_eq!(replay(b"\x1b(=\x1b[?1h"), "\x1b[?1h");
        assert_eq!(replay(b"\x1b#8"), "");
    }

    #[test]
    fn cancel_aborts_sequence() {
        assert_eq!(replay(b"\x1b[?10\x1800h"), "");
        assert_eq!(replay(b"\x1b[?1000\x1ah"), "");
    }

    #[test]
    fn escape_restarts_sequence() {
        assert_eq!(replay(b"\x1b[?10\x1b[?2004h"), "\x1b[?2004h");
    }

    #[test]
    fn embedded_controls_executed() {
        assert_eq!(replay(b"\x1b[?10\r\n00h"), "\x1b[?1000h");
    }

    #[test]
    fn huge_parameters_saturate() {
        assert_eq!(replay(b"\x1b[?99999999999h\x1b[?25l"), "\x1b[?25l");
    }

    #[test]
    fn too_many_parameters_ignored() {
        let mut data = b"\x1b[?".to_vec();
        data.extend("1;".repeat(MAX_PARAMS).bytes());
        data.extend(b"1000h");
        assert_eq!(replay(&data), "");
    }

    #[test]
    fn empty_parameters_skipped() {
        assert_eq!(replay(b"\x1b[?;1000;h"), "\x1b[?1000h");
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        scanned(&data).replay();
    }
}
