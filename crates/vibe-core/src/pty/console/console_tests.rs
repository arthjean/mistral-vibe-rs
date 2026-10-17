//! The console's exchange with its host never reaches a session, and nothing
//! the child wrote is lost to it. The byte runs are the ones ConPTY wrote on
//! Windows 11 for `printf OK` under Git Bash.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use super::{ConsoleReader, SharedWriter};

const PROLOGUE: &[u8] = b"\x1b[6n\x1b[?9001h\x1b[?1004h\x1b[m\x1b]0;C:\\Program Files\\Git\\usr\\bin\\bash.exe\x07\x1b[?25h";
const EPILOGUE: &[u8] = b"\x1b[?9001l\x1b[?1004l";

/// Hands its chunks out one read at a time, as a pipe would.
struct Chunks(Vec<Vec<u8>>);

impl Read for Chunks {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.0.is_empty() {
            return Ok(0);
        }
        let chunk = self.0.remove(0);
        let count = chunk.len().min(buffer.len());
        buffer[..count].copy_from_slice(&chunk[..count]);
        if count < chunk.len() {
            self.0.insert(0, chunk[count..].to_vec());
        }
        Ok(count)
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads everything `chunks` yields through a console reader, returning what
/// the session sees and what was written back to the console.
fn drain(chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
    let sink = Sink::default();
    let writer: SharedWriter = Arc::new(Mutex::new(Some(Box::new(sink.clone()))));
    let mut reader = ConsoleReader::new(
        Box::new(Chunks(chunks.iter().map(|chunk| chunk.to_vec()).collect())),
        writer,
    );
    let mut seen = Vec::new();
    reader.read_to_end(&mut seen).unwrap();
    let answered = sink.0.lock().unwrap().clone();
    (seen, answered)
}

#[test]
fn the_console_exchange_is_answered_and_dropped_and_the_output_kept() {
    let (seen, answered) = drain(&[PROLOGUE, b"OK\r\n", EPILOGUE]);
    assert_eq!(String::from_utf8_lossy(&seen), "OK\r\n");
    assert_eq!(answered, b"\x1b[1;1R");
}

#[test]
fn a_request_split_across_reads_is_still_answered() {
    let (seen, answered) = drain(&[b"\x1b[", b"6", b"n", b"hi"]);
    assert_eq!(seen, b"hi");
    assert_eq!(answered, b"\x1b[1;1R");
}

/// Once the child has written, its own colors, cursor moves and titles are its
/// output: only the host modes are still the console's.
#[test]
fn the_childs_own_sequences_pass_through() {
    let child = b"a\x1b[31mred\x1b[m\x1b]0;mine\x07\x1b[?25h\x1b[?1004h";
    let (seen, answered) = drain(&[PROLOGUE, child]);
    assert_eq!(seen, b"a\x1b[31mred\x1b[m\x1b]0;mine\x07\x1b[?25h");
    assert_eq!(answered, b"\x1b[1;1R");
}

/// A sequence the console never finished belongs to the child, and is handed
/// over when the console closes rather than lost.
#[test]
fn an_unfinished_sequence_is_kept_at_the_end() {
    let (seen, _) = drain(&[b"tail\x1b[3"]);
    assert_eq!(seen, b"tail\x1b[3");
}

/// The cap that keeps a runaway sequence from being held forever releases it
/// as written.
#[test]
fn an_overlong_sequence_is_released_as_written() {
    let mut long = b"\x1b]0;".to_vec();
    long.extend(std::iter::repeat_n(b'x', super::MAX_SEQUENCE + 10));
    let (seen, _) = drain(&[&long]);
    assert_eq!(seen, long);
}
