//! Keyboard and gamepad input read directly from evdev, for running without a display server.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    PlatformInput,
};

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_REP: usize = 0x14;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const KEY_ENTER: usize = 28;
const KEY_POWER: usize = 116;
const BTN_SOUTH: usize = 0x130;
const KEY_COUNT: usize = 0x300;

/// Repeat timing for held keys on devices whose driver does not repeat them (typically
/// gamepads). Slower than keyboard defaults so D-pad list navigation stays precise.
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
struct Key {
    name: &'static str,
    /// Characters typed without and with shift, for keys that produce text.
    characters: Option<(char, char)>,
}

impl Key {
    const fn named(name: &'static str) -> Self {
        Self {
            name,
            characters: None,
        }
    }

    const fn printable(name: &'static str, plain: char, shifted: char) -> Self {
        Self {
            name,
            characters: Some((plain, shifted)),
        }
    }
}

/// Maps Linux key codes (US layout) to gpui's key names. Gamepad buttons keep their Linux
/// names, since drivers disagree on which physical button each code stands for.
fn key_for_code(code: u16) -> Option<Key> {
    let key = match code {
        1 => Key::named("escape"),
        2 => Key::printable("1", '1', '!'),
        3 => Key::printable("2", '2', '@'),
        4 => Key::printable("3", '3', '#'),
        5 => Key::printable("4", '4', '$'),
        6 => Key::printable("5", '5', '%'),
        7 => Key::printable("6", '6', '^'),
        8 => Key::printable("7", '7', '&'),
        9 => Key::printable("8", '8', '*'),
        10 => Key::printable("9", '9', '('),
        11 => Key::printable("0", '0', ')'),
        12 => Key::printable("-", '-', '_'),
        13 => Key::printable("=", '=', '+'),
        14 => Key::named("backspace"),
        15 => Key::named("tab"),
        16 => Key::printable("q", 'q', 'Q'),
        17 => Key::printable("w", 'w', 'W'),
        18 => Key::printable("e", 'e', 'E'),
        19 => Key::printable("r", 'r', 'R'),
        20 => Key::printable("t", 't', 'T'),
        21 => Key::printable("y", 'y', 'Y'),
        22 => Key::printable("u", 'u', 'U'),
        23 => Key::printable("i", 'i', 'I'),
        24 => Key::printable("o", 'o', 'O'),
        25 => Key::printable("p", 'p', 'P'),
        26 => Key::printable("[", '[', '{'),
        27 => Key::printable("]", ']', '}'),
        28 => Key::named("enter"),
        30 => Key::printable("a", 'a', 'A'),
        31 => Key::printable("s", 's', 'S'),
        32 => Key::printable("d", 'd', 'D'),
        33 => Key::printable("f", 'f', 'F'),
        34 => Key::printable("g", 'g', 'G'),
        35 => Key::printable("h", 'h', 'H'),
        36 => Key::printable("j", 'j', 'J'),
        37 => Key::printable("k", 'k', 'K'),
        38 => Key::printable("l", 'l', 'L'),
        39 => Key::printable(";", ';', ':'),
        40 => Key::printable("'", '\'', '"'),
        41 => Key::printable("`", '`', '~'),
        43 => Key::printable("\\", '\\', '|'),
        44 => Key::printable("z", 'z', 'Z'),
        45 => Key::printable("x", 'x', 'X'),
        46 => Key::printable("c", 'c', 'C'),
        47 => Key::printable("v", 'v', 'V'),
        48 => Key::printable("b", 'b', 'B'),
        49 => Key::printable("n", 'n', 'N'),
        50 => Key::printable("m", 'm', 'M'),
        51 => Key::printable(",", ',', '<'),
        52 => Key::printable(".", '.', '>'),
        53 => Key::printable("/", '/', '?'),
        57 => Key::printable("space", ' ', ' '),
        59 => Key::named("f1"),
        60 => Key::named("f2"),
        61 => Key::named("f3"),
        62 => Key::named("f4"),
        63 => Key::named("f5"),
        64 => Key::named("f6"),
        65 => Key::named("f7"),
        66 => Key::named("f8"),
        67 => Key::named("f9"),
        68 => Key::named("f10"),
        87 => Key::named("f11"),
        88 => Key::named("f12"),
        102 => Key::named("home"),
        103 => Key::named("up"),
        104 => Key::named("pageup"),
        105 => Key::named("left"),
        106 => Key::named("right"),
        107 => Key::named("end"),
        108 => Key::named("down"),
        109 => Key::named("pagedown"),
        110 => Key::named("insert"),
        111 => Key::named("delete"),
        113 => Key::named("mute"),
        114 => Key::named("volumedown"),
        115 => Key::named("volumeup"),
        116 => Key::named("power"),
        158 => Key::named("back"),
        0x130 => Key::named("btn_south"),
        0x131 => Key::named("btn_east"),
        0x132 => Key::named("btn_c"),
        0x133 => Key::named("btn_north"),
        0x134 => Key::named("btn_west"),
        0x135 => Key::named("btn_z"),
        0x136 => Key::named("btn_tl"),
        0x137 => Key::named("btn_tr"),
        0x138 => Key::named("btn_tl2"),
        0x139 => Key::named("btn_tr2"),
        0x13a => Key::named("btn_select"),
        0x13b => Key::named("btn_start"),
        0x13c => Key::named("btn_mode"),
        0x13d => Key::named("btn_thumbl"),
        0x13e => Key::named("btn_thumbr"),
        0x220 => Key::named("up"),
        0x221 => Key::named("down"),
        0x222 => Key::named("left"),
        0x223 => Key::named("right"),
        _ => return None,
    };
    Some(key)
}

fn modifier_flag(modifiers: &mut Modifiers, code: u16) -> Option<&mut bool> {
    match code {
        29 | 97 => Some(&mut modifiers.control),
        42 | 54 => Some(&mut modifiers.shift),
        56 | 100 => Some(&mut modifiers.alt),
        125 | 126 => Some(&mut modifiers.platform),
        _ => None,
    }
}

/// `_IOC(_IOC_READ, 'E', number, len)`, the encoding of evdev's read ioctls.
const fn evdev_read_request(number: u64, len: usize) -> u64 {
    const IOC_READ: u64 = 2;
    (IOC_READ << 30) | ((len as u64) << 16) | ((b'E' as u64) << 8) | number
}

/// `EVIOCGBIT`: which codes of `event_type` (or which event types, for 0) the device reports.
fn query_bits(file: &File, event_type: u64, bits: &mut [u8]) -> bool {
    let request = evdev_read_request(0x20 + event_type, bits.len());
    unsafe { libc::ioctl(file.as_raw_fd(), request as _, bits.as_mut_ptr()) >= 0 }
}

fn device_name(file: &File) -> String {
    let mut name = [0u8; 256];
    let request = evdev_read_request(0x06, name.len());
    let len = unsafe { libc::ioctl(file.as_raw_fd(), request as _, name.as_mut_ptr()) };
    if len <= 0 {
        return "unknown".into();
    }
    let name = &name[..(len as usize).min(name.len())];
    let name = name.split(|byte| *byte == 0).next().unwrap_or(name);
    String::from_utf8_lossy(name).into_owned()
}

fn has_bit(bits: &[u8], bit: usize) -> bool {
    bits.get(bit / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

pub(crate) struct EvdevDevice {
    pub(crate) name: String,
    /// Whether the driver repeats held keys itself (EV_REP); otherwise they are repeated here.
    kernel_repeat: bool,
    /// The direction currently pressed on each D-pad hat axis.
    hat: [Option<Key>; 2],
}

/// Opens every keyboard, gamepad and power button under /dev/input. Other key sources are
/// skipped: extra key drivers on handhelds can report the same physical press a second time.
pub(crate) fn open_devices() -> Vec<(File, EvdevDevice)> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("Cannot list input devices: {error}");
            return Vec::new();
        }
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect();
    paths.sort();
    paths.iter().filter_map(|path| open_device(path)).collect()
}

fn open_device(path: &Path) -> Option<(File, EvdevDevice)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .inspect_err(|error| log::warn!("Cannot open {}: {error}", path.display()))
        .ok()?;

    let mut event_types = [0u8; EV_REP / 8 + 1];
    let mut keys = [0u8; KEY_COUNT / 8];
    if !query_bits(&file, 0, &mut event_types) || !query_bits(&file, EV_KEY.into(), &mut keys) {
        return None;
    }
    let name = device_name(&file);
    if ![KEY_ENTER, BTN_SOUTH, KEY_POWER]
        .iter()
        .any(|key| has_bit(&keys, *key))
    {
        log::info!("Ignoring input device {} ({name})", path.display());
        return None;
    }
    log::info!("Using input device {} ({name})", path.display());

    Some((
        file,
        EvdevDevice {
            name,
            kernel_repeat: has_bit(&event_types, EV_REP),
            hat: [None, None],
        },
    ))
}

impl EvdevDevice {
    /// Drains the device's pending events, translating them into gpui input.
    pub(crate) fn read_events(
        &mut self,
        mut file: &File,
        keyboard: &mut KeyboardState,
        output: &mut Vec<PlatformInput>,
    ) -> io::Result<()> {
        const EVENT_SIZE: usize = std::mem::size_of::<libc::input_event>();
        let mut buffer = [0u8; EVENT_SIZE * 64];
        loop {
            let len = match file.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(len) => len,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            for record in buffer[..len].chunks_exact(EVENT_SIZE) {
                // Safety: the kernel only writes whole `input_event` records, and any bit
                // pattern is a valid `input_event`.
                let event = unsafe {
                    std::ptr::read_unaligned(record.as_ptr().cast::<libc::input_event>())
                };
                self.handle_event(event.type_, event.code, event.value, keyboard, output);
            }
        }
    }

    fn handle_event(
        &mut self,
        event_type: u16,
        code: u16,
        value: i32,
        keyboard: &mut KeyboardState,
        output: &mut Vec<PlatformInput>,
    ) {
        match event_type {
            EV_KEY => keyboard.handle_key(code, value, !self.kernel_repeat, output),
            EV_ABS if code == ABS_HAT0X || code == ABS_HAT0Y => {
                let key = match (code, value.signum()) {
                    (ABS_HAT0X, -1) => Some(Key::named("left")),
                    (ABS_HAT0X, 1) => Some(Key::named("right")),
                    (ABS_HAT0Y, -1) => Some(Key::named("up")),
                    (ABS_HAT0Y, 1) => Some(Key::named("down")),
                    _ => None,
                };
                let Some(pressed) = self.hat.get_mut(usize::from(code - ABS_HAT0X)) else {
                    return;
                };
                if *pressed == key {
                    return;
                }
                if let Some(previous) = pressed.take() {
                    keyboard.release(previous, output);
                }
                if let Some(key) = key {
                    keyboard.press(key, true, output);
                }
                *pressed = key;
            }
            _ => {}
        }
    }
}

struct Repeat {
    key: Key,
    next: Instant,
}

/// Modifier and key-repeat state shared by all input devices.
#[derive(Default)]
pub(crate) struct KeyboardState {
    modifiers: Modifiers,
    repeat: Option<Repeat>,
}

impl KeyboardState {
    fn handle_key(
        &mut self,
        code: u16,
        value: i32,
        software_repeat: bool,
        output: &mut Vec<PlatformInput>,
    ) {
        if let Some(flag) = modifier_flag(&mut self.modifiers, code) {
            let pressed = value != 0;
            if std::mem::replace(flag, pressed) != pressed {
                output.push(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: self.modifiers,
                    capslock: Capslock::default(),
                }));
            }
            return;
        }
        let Some(key) = key_for_code(code) else {
            return;
        };
        match value {
            0 => self.release(key, output),
            1 => self.press(key, software_repeat, output),
            2 => output.push(self.key_down(key, true)),
            _ => {}
        }
    }

    fn press(&mut self, key: Key, software_repeat: bool, output: &mut Vec<PlatformInput>) {
        output.push(self.key_down(key, false));
        self.repeat = software_repeat.then(|| Repeat {
            key,
            next: Instant::now() + REPEAT_DELAY,
        });
    }

    fn release(&mut self, key: Key, output: &mut Vec<PlatformInput>) {
        if self
            .repeat
            .as_ref()
            .is_some_and(|repeat| repeat.key == key)
        {
            self.repeat = None;
        }
        output.push(PlatformInput::KeyUp(KeyUpEvent {
            keystroke: self.keystroke(key),
        }));
    }

    /// Returns a repeat of the held key once it is due, for keys the driver does not repeat.
    pub(crate) fn due_repeat(&mut self, now: Instant) -> Option<PlatformInput> {
        let repeat = self.repeat.as_mut()?;
        if now < repeat.next {
            return None;
        }
        repeat.next = now + REPEAT_INTERVAL;
        let key = repeat.key;
        Some(self.key_down(key, true))
    }

    fn key_down(&self, key: Key, is_held: bool) -> PlatformInput {
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke: self.keystroke(key),
            is_held,
            prefer_character_input: false,
        })
    }

    fn keystroke(&self, key: Key) -> Keystroke {
        let modifiers = self.modifiers;
        let types_text = !(modifiers.control || modifiers.alt || modifiers.platform);
        let key_char = key
            .characters
            .filter(|_| types_text)
            .map(|(plain, shifted)| (if modifiers.shift { shifted } else { plain }).to_string());
        Keystroke {
            modifiers,
            key: key.name.to_owned(),
            key_char,
        }
    }
}
