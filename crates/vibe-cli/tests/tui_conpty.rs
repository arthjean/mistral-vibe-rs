//! The TUI driven through a Windows pseudoconsole.
//!
//! What is written here reaches the TUI as a terminal's input does on Windows:
//! the console host turns the VT it receives into the key events crossterm
//! reads with `ReadConsoleInput`, one per character, which is how Windows
//! Terminal and conhost deliver a paste. A whole key event, modifiers included,
//! travels as a win32-input-mode sequence (`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`),
//! the encoding `portable-pty` opens the console with, which is how AltGr
//! reaches the TUI from a French AZERTY keyboard.
//!
//! The composer is read back through the external editor: `VISUAL` names a
//! batch fixture that copies the file it is handed beside itself, so each test
//! asserts the exact text the composer held.

#![cfg(windows)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "console integration failures must terminate with their captured transcript"
)]

mod support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const STEP: Duration = Duration::from_secs(30);

/// `LEFT_CTRL_PRESSED`, and AltGr as Windows reports it: `RIGHT_ALT_PRESSED`
/// with `LEFT_CTRL_PRESSED`.
const LEFT_CTRL: u32 = 0x0008;
const ALTGR: u32 = 0x0001 | LEFT_CTRL;

/// One key pressed and released, as a win32-input-mode terminal sends it.
fn win32_key(virtual_key: u32, scan_code: u32, character: char, state: u32) -> Vec<u8> {
    let unicode = u32::from(character);
    format!(
        "\x1b[{virtual_key};{scan_code};{unicode};1;{state};1_\x1b[{virtual_key};{scan_code};{unicode};0;{state};1_"
    )
    .into_bytes()
}

/// `Ctrl+G`, the external editor chord.
fn ctrl_g() -> Vec<u8> {
    win32_key(0x47, 0x22, '\u{7}', LEFT_CTRL)
}

/// The text a transcript painted, escape sequences removed.
fn printable(transcript: &[u8]) -> String {
    let text = String::from_utf8_lossy(transcript);
    let mut out = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                Some('[') => {
                    for byte in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&byte) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for byte in characters.by_ref() {
                        if byte == '\u{7}' || byte == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            character if character.is_control() => out.push(' '),
            character => out.push(character),
        }
    }
    out
}

/// A batch editor that copies the prompt it is handed to `captured.md` and its
/// first argument to `argument.txt`, both beside itself.
fn capturing_editor(directory: &Path) -> PathBuf {
    let script = directory.join("capture editor.cmd");
    std::fs::write(
        &script,
        "@echo off\r\n\
         echo %~1> \"%~dp0argument.txt\"\r\n\
         copy /y \"%~2\" \"%~dp0captured.partial\" >nul\r\n\
         move /y \"%~dp0captured.partial\" \"%~dp0captured.md\" >nul\r\n",
    )
    .expect("editor fixture");
    script
}

struct Console {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Box<dyn Write + Send>,
    receiver: Receiver<Vec<u8>>,
    transcript: Vec<u8>,
    directory: PathBuf,
}

impl Console {
    /// Starts `vibe` in `directory/workspace` with its home in
    /// `directory/home` and `VISUAL` set to `visual`.
    fn start(directory: &Path, visual: &str) -> Self {
        let workspace = directory.join("workspace");
        let home = directory.join("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&home).expect("home");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 160,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pseudoconsole opens");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_vibe"));
        command.args(["--trust", "--api-base", "http://127.0.0.1:9/v1"]);
        command.cwd(&workspace);
        command.env_clear();
        for name in std::iter::once("PATH").chain(support::WINDOWS_BASE) {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("USERPROFILE", &home);
        command.env("HOME", &home);
        command.env("VIBE_HOME", home.join(".vibe"));
        command.env("MISTRAL_API_KEY", "fixture");
        // Update discovery must never leave the machine during tests.
        command.env("VIBE_UPDATE_BASE_URL", "http://127.0.0.1:9/v1");
        command.env("NO_COLOR", "1");
        command.env("VISUAL", visual);
        let child = pair.slave.spawn_command(command).expect("TUI starts");
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().expect("console reads");
        let writer = pair.master.take_writer().expect("console writes");
        let (mut reader, writer) = vibe_core::pty::pseudoconsole_streams(reader, writer);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            while let Ok(count @ 1..) = reader.read(&mut buffer) {
                if sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut console = Self {
            child,
            master: Some(pair.master),
            writer,
            receiver,
            transcript: Vec::new(),
            directory: directory.to_path_buf(),
        };
        console.wait_for("Type /help for more information");
        console
    }

    fn workspace(&self) -> PathBuf {
        self.directory.join("workspace")
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("console input writes");
        self.writer.flush().expect("console input flushes");
    }

    /// Types `text` one key at a time, slower than any burst.
    fn type_slowly(&mut self, text: &str) {
        for character in text.chars() {
            let mut encoded = [0_u8; 4];
            self.write(character.encode_utf8(&mut encoded).as_bytes());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Waits until the painted text holds `pattern`.
    fn wait_for(&mut self, pattern: &str) {
        let deadline = Instant::now() + STEP;
        while !printable(&self.transcript).contains(pattern) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => self.transcript.extend(chunk),
                Err(error) => panic!(
                    "the console stopped ({error}) before painting {pattern:?}: {}",
                    printable(&self.transcript)
                ),
            }
        }
    }

    /// Opens the external editor and answers the prompt it was handed.
    fn composer_text(&mut self) -> String {
        let captured = self.directory.join("captured.md");
        let _ = std::fs::remove_file(&captured);
        self.write(&ctrl_g());
        let deadline = Instant::now() + STEP;
        loop {
            if let Ok(text) = std::fs::read_to_string(&captured) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the editor never received the prompt: {}",
                printable(&self.transcript)
            );
            while let Ok(chunk) = self.receiver.try_recv() {
                self.transcript.extend(chunk);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Some(pid) = self.child.process_id() {
            let _ = vibe_core::pty::kill_tree(pid);
        }
        let _ = self.child.wait();
        // Closing the pseudoconsole blocks until its host is gone, so it never
        // runs on the thread that has to report the result.
        if let Some(master) = self.master.take() {
            std::thread::spawn(move || drop(master));
        }
    }
}

fn session(visual: impl FnOnce(&Path) -> String) -> (tempfile::TempDir, Console) {
    let directory = tempfile::tempdir().expect("console fixture");
    let visual = visual(directory.path());
    let console = Console::start(directory.path(), &visual);
    (directory, console)
}

fn quoted_editor(directory: &Path) -> String {
    format!("\"{}\" --from-vibe", capturing_editor(directory).display())
}

#[test]
fn a_quoted_editor_path_keeps_its_backslashes_and_receives_its_argument() {
    let (directory, mut console) = session(quoted_editor);
    console.type_slowly("draft");
    assert_eq!(console.composer_text(), "draft");
    let argument =
        std::fs::read_to_string(directory.path().join("argument.txt")).expect("editor argument");
    assert_eq!(argument.trim_end(), "--from-vibe");
}

#[test]
fn an_editor_that_cannot_start_is_named_on_screen() {
    let (_directory, mut console) = session(|_| "vibe-missing-editor-0 --wait".to_owned());
    console.type_slowly("keep");
    console.write(&ctrl_g());
    console.wait_for("`vibe-missing-editor-0` could not start");
}

#[test]
fn altgr_zero_on_azerty_types_an_at_sign_that_opens_mention_completion() {
    let (_directory, mut console) = session(quoted_editor);
    std::fs::write(console.workspace().join("alpha-marker.txt"), "").expect("mention target");
    // French AZERTY: AltGr+0 is `@` (`VK_0`, scan code 0x0B).
    console.write(&win32_key(0x30, 0x0B, '@', ALTGR));
    console.wait_for("alpha-marker.txt");
    assert_eq!(console.composer_text(), "@");
}

#[test]
fn a_ctrl_alt_letter_is_not_typed() {
    let (_directory, mut console) = session(quoted_editor);
    console.type_slowly("x");
    // `Ctrl+Alt+Q` on a US layout reports its own letter.
    console.write(&win32_key(0x51, 0x10, 'q', ALTGR));
    assert_eq!(console.composer_text(), "x");
}

#[test]
fn a_fifty_line_paste_lands_in_the_composer_and_submits_nothing() {
    let (_directory, mut console) = session(quoted_editor);
    let lines = (1..=50)
        .map(|line| format!("    at frame {line:02} (stack.rs:{line})"))
        .collect::<Vec<_>>();
    console.write(lines.join("\r").as_bytes());
    assert_eq!(console.composer_text(), lines.join("\n"));
}

#[test]
fn a_ten_thousand_character_paste_lands_whole() {
    let (_directory, mut console) = session(quoted_editor);
    let pasted = "0123456789".repeat(1_000);
    console.write(pasted.as_bytes());
    assert_eq!(console.composer_text(), pasted);
}

/// crossterm reads the console one record per call, about 75 us each under
/// ConPTY, so the 20,000 records of this paste take 1.5 s at best.
#[test]
fn a_ten_thousand_character_paste_lands_within_two_seconds() {
    let (_directory, mut console) = session(quoted_editor);
    let pasted = "0123456789".repeat(1_000);
    let started = Instant::now();
    console.write(pasted.as_bytes());
    let composer = console.composer_text();
    let elapsed = started.elapsed();
    assert_eq!(composer, pasted);
    assert!(
        elapsed < Duration::from_secs(2),
        "the paste reached the editor after {elapsed:?}"
    );
}

#[test]
fn tab_and_escape_inside_a_paste_neither_complete_nor_cancel() {
    let (_directory, mut console) = session(quoted_editor);
    std::fs::write(console.workspace().join("alpha-marker.txt"), "").expect("mention target");
    console.write(b"@alp\tnext\x1bend");
    assert_eq!(console.composer_text(), "@alp\tnextend");
}

#[test]
fn a_paste_ending_with_a_newline_keeps_it() {
    let (_directory, mut console) = session(quoted_editor);
    console.write(b"first\rsecond\r");
    // The editor trims what it hands back, never what it is handed.
    assert_eq!(console.composer_text(), "first\nsecond\n");
}

#[test]
fn typing_with_pauses_and_enter_submits_the_prompt() {
    let (_directory, mut console) = session(quoted_editor);
    console.type_slowly("/copy\r");
    console.wait_for("/ copy");
    assert_eq!(console.composer_text(), "");
}

#[test]
fn a_dropped_windows_path_with_a_space_becomes_a_quoted_mention() {
    let (_directory, mut console) = session(quoted_editor);
    let image = console.workspace().join("image one.png");
    std::fs::write(&image, b"image").expect("image fixture");
    let image = image.display().to_string();
    console.write(format!("\"{image}\"").as_bytes());
    console.wait_for("@'");
    assert_eq!(console.composer_text(), format!("@'{image}'"));
}

#[test]
fn an_unquoted_drive_path_typed_in_a_prompt_becomes_an_image_mention() {
    let (_directory, mut console) = session(quoted_editor);
    let image = console.workspace().join("x.png");
    std::fs::write(&image, b"image").expect("image fixture");
    let image = image.display().to_string();
    console.type_slowly(&image);
    console.wait_for("@'");
    assert_eq!(console.composer_text(), format!("@'{image}'"));
}
