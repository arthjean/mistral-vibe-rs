#![cfg(unix)]

use std::time::{Duration, Instant};

use super::*;
use crate::world::Setup;

const STEP: Duration = Duration::from_secs(20);

fn shell(world: &World, script: &str, cols: u16, rows: u16) -> Tui {
    world
        .program("sh")
        .arg("-c")
        .arg(script)
        .spawn_tui(cols, rows)
        .unwrap()
}

#[test]
fn the_screen_model_exposes_text_attributes_cursor_and_the_alternate_screen() {
    let world = World::new(Setup::default()).unwrap();
    let mut tui = shell(
        &world,
        "printf '\\033[1mbold\\033[0m plain\\n'; printf '\\033[?1049h\\033[3;7Halt'; sleep 30",
        40,
        10,
    );
    let screen = tui.wait_text("alt", STEP).unwrap();
    assert!(screen.alternate());
    assert_eq!(screen.size(), (40, 10));
    assert_eq!(screen.cursor(), (2, 9));
    assert_eq!(screen.find("alt"), Some((2, 6)));
    assert!(!screen.contains("plain"), "{}", screen.text());
    tui.kill();

    let mut tui = shell(
        &world,
        "printf '\\033[1mbold\\033[0m plain'; sleep 30",
        40,
        10,
    );
    let screen = tui.wait_text("bold plain", STEP).unwrap();
    assert!(!screen.alternate());
    assert!(screen.cell(0, 0).unwrap().bold());
    assert!(!screen.cell(0, 5).unwrap().bold());
}

#[test]
fn cursor_position_and_device_attribute_requests_are_answered() {
    let world = World::new(Setup::default()).unwrap();
    let script = "stty -icanon -echo min 1; printf '\\033[2;5H\\033[6n'; \
                  cpr=$(dd bs=1 count=6 2>/dev/null); printf '\\033[c'; \
                  da=$(dd bs=1 count=9 2>/dev/null); \
                  printf '\\033[5;1Hcpr %s da %s' \"${cpr#?}\" \"${da#?}\"; sleep 30";
    let mut tui = shell(&world, script, 40, 10);
    let screen = tui.wait_text("cpr ", STEP).unwrap();
    assert!(
        screen.contains("cpr [2;5R da [?62;22c"),
        "{}",
        screen.text()
    );
}

#[test]
fn an_expired_wait_quotes_the_screen_and_the_raw_bytes() {
    let world = World::new(Setup::default()).unwrap();
    let mut tui = shell(&world, "printf 'still waiting'; sleep 30", 40, 10);
    tui.wait_text("still waiting", STEP).unwrap();
    let failure = tui
        .wait_text("never shown", Duration::from_millis(200))
        .unwrap_err();
    assert!(
        failure.0.contains("did not show \"never shown\""),
        "{failure}"
    );
    assert!(
        failure.0.contains("final screen:\nstill waiting"),
        "{failure}"
    );
    assert!(failure.0.contains("last 8192 bytes"), "{failure}");
}

#[test]
fn a_child_that_exits_fails_the_wait_within_two_seconds() {
    let world = World::new(Setup::default()).unwrap();
    let mut tui = shell(&world, "echo bye; exit 3", 40, 10);
    let started = Instant::now();
    let failure = tui.wait_text("never shown", STEP).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "{failure}");
    assert!(failure.0.contains("exited (exit code 3)"), "{failure}");
    assert!(failure.0.contains("bye"), "{failure}");
}

#[test]
fn keys_reach_the_child_as_xterm_sends_them() {
    let world = World::new(Setup::default()).unwrap();
    let keys = [
        Key::Text("a".to_owned()),
        Key::Enter,
        Key::Tab,
        Key::BackTab,
        Key::Backspace,
        Key::Up,
        Key::Down,
        Key::Right,
        Key::Left,
        Key::Ctrl('C'),
        Key::Paste("p".to_owned()),
        Key::Esc,
    ];
    let expected: Vec<u8> = keys.iter().flat_map(|key| key.bytes(false, true)).collect();
    // The shell turns bracketed paste on, so the paste arrives wrapped.
    let script = format!(
        "stty raw -echo; printf '\\033[?2004hready'; bytes=$(dd bs=1 count={} 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
         printf '\\r\\nhex %s end' \"$bytes\"; sleep 30",
        expected.len()
    );
    let mut tui = shell(&world, &script, 200, 10);
    tui.wait_text("ready", STEP).unwrap();
    tui.send(&keys).unwrap();
    let screen = tui.wait_text(" end", STEP).unwrap();
    let hex: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
    assert!(
        screen.contains(&format!("hex {hex} end")),
        "{}",
        screen.text()
    );
    assert_eq!(Key::Up.bytes(true, false), b"\x1bOA");
    assert_eq!(Key::Ctrl('c').bytes(false, false), [3]);
    assert_eq!(Key::Paste("x".to_owned()).bytes(false, false), b"x");
    assert_eq!(
        Key::Paste("x".to_owned()).bytes(false, true),
        b"\x1b[200~x\x1b[201~"
    );
}

#[test]
fn a_resize_changes_the_terminal_and_the_screen_model() {
    let world = World::new(Setup::default()).unwrap();
    let script = "trap 'printf \"\\033[2J\\033[1;1Hsize %s\" \"$(stty size)\"' WINCH; \
                  printf ready; while :; do sleep 0.05; done";
    let mut tui = shell(&world, script, 80, 24);
    tui.wait_text("ready", STEP).unwrap();
    tui.resize(120, 30).unwrap();
    let screen = tui.wait_text("size 30 120", STEP).unwrap();
    assert_eq!(screen.size(), (120, 30));
}

#[test]
fn window_titles_are_captured() {
    let world = World::new(Setup::default()).unwrap();
    let mut tui = shell(
        &world,
        "printf '\\033]0;first\\007\\033]2;second\\033\\\\done'; sleep 30",
        40,
        10,
    );
    tui.wait_text("done", STEP).unwrap();
    assert_eq!(tui.titles(), ["first", "second"]);
}

#[test]
fn at_most_two_sessions_are_live_at_once() {
    let world = World::new(Setup::default()).unwrap();
    let threads: Vec<_> = (0..5)
        .map(|_| {
            let world = world.clone();
            std::thread::spawn(move || {
                let mut tui = shell(&world, "sleep 0.3", 20, 5);
                tui.wait_exit(STEP).unwrap()
            })
        })
        .collect();
    for thread in threads {
        assert!(thread.join().unwrap().success);
    }
    assert!(peak_live() <= LIVE_PTYS, "{} live at once", peak_live());
    assert!(peak_live() >= 1);
}
