#![cfg(unix)]

use std::time::{Duration, Instant};

use crate::world::{Setup, World};

#[test]
fn stdin_is_the_null_device_unless_piped() {
    let world = World::new(Setup::default()).unwrap();
    let output = world
        .program("sh")
        .args(["-c", "cat; echo done"])
        .run()
        .unwrap();
    assert_eq!(output.stdout, "done\n");
    let mut child = world
        .program("sh")
        .args(["-c", "read line; echo got $line"])
        .stdin_piped()
        .spawn()
        .unwrap();
    child.write_line("ping").unwrap();
    assert_eq!(child.wait().unwrap().stdout, "got ping\n");
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_leads_its_own_process_group() {
    let world = World::new(Setup::default()).unwrap();
    let child = world
        .program("sh")
        .args(["-c", "cut -d' ' -f5 /proc/$$/stat"])
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait().unwrap();
    assert_eq!(output.stdout.trim(), pid.to_string());
}

#[test]
fn an_expired_deadline_kills_the_group_and_quotes_the_output() {
    let world = World::new(Setup::default()).unwrap();
    let started = Instant::now();
    let error = world
        .program("sh")
        .args([
            "-c",
            "echo started; echo complaint >&2; sleep 30 & sleep 30",
        ])
        .deadline(Duration::from_millis(300))
        .run()
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(error.0.contains("process group was killed"), "{error}");
    assert!(
        error.0.contains("started") && error.0.contains("complaint"),
        "{error}"
    );
    assert_eq!(world.finish(), Vec::<String>::new());
}

#[test]
fn a_child_that_ignores_sigterm_is_killed_after_two_seconds() {
    let world = World::new(Setup::default()).unwrap();
    let mut child = world
        .program("sh")
        .args([
            "-c",
            "trap '' TERM; echo ready; while :; do sleep 0.1; done",
        ])
        .spawn()
        .unwrap();
    child
        .wait_for_stdout(|out| out.contains("ready"), Duration::from_secs(5))
        .unwrap();
    let started = Instant::now();
    drop(child);
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(1900), "killed after {took:?}");
    assert!(took < Duration::from_secs(5), "killed after {took:?}");
    assert_eq!(world.finish(), Vec::<String>::new());
}

#[cfg(target_os = "linux")]
#[test]
fn a_process_left_behind_fails_the_trial_and_is_killed() {
    let world = World::new(Setup::default()).unwrap();
    let output = world
        .program("sh")
        .args(["-c", "sleep 300 >/dev/null 2>&1 & echo $!"])
        .run()
        .unwrap();
    let pid: u32 = output.stdout.trim().parse().unwrap();
    let failures = world.finish();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains(&format!("process {pid} (sleep 300)")),
        "{failures:?}"
    );
    assert!(failures[0].contains("survived teardown"), "{failures:?}");
    let until = Instant::now() + Duration::from_secs(2);
    while std::path::Path::new(&format!("/proc/{pid}/environ")).exists()
        && std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|e| !e.is_empty())
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(crate::process::survivors(world.name()).is_empty());
}

#[test]
fn next_line_reads_stdout_line_by_line() {
    let world = World::new(Setup::default()).unwrap();
    let mut child = world
        .program("sh")
        .args(["-c", "echo one; echo two; sleep 0.2; echo three"])
        .spawn()
        .unwrap();
    for expected in ["one", "two", "three"] {
        assert_eq!(child.next_line(Duration::from_secs(5)).unwrap(), expected);
    }
    assert!(
        child
            .next_line(Duration::from_secs(5))
            .unwrap_err()
            .0
            .contains("exited")
    );
}

#[test]
fn the_time_scale_multiplies_deadlines() {
    assert_eq!(crate::scale_from(None), 1.0);
    assert_eq!(crate::scale_from(Some("2.5")), 2.5);
    assert_eq!(crate::scale_from(Some("0")), 1.0);
    assert_eq!(crate::scale_from(Some("fast")), 1.0);
}
