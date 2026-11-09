//! The keystroke scripts of the black-box TUI families, and the worlds they
//! run in. Every string a script types, every stand-in answer and every
//! seeded name is authored here, which is what lets a normalized row that
//! holds only these keep its text.

use serde_json::json;
use vibe_e2e::screens::{Script, Step};
use vibe_e2e::{Key, Outcome, Reply, World};

/// Where a world is prepared before the subject starts: seeded files and the
/// stand-in's answers.
pub type Prepare = fn(&World) -> Outcome;

/// One script of a family.
pub struct Scenario {
    /// The replay heading the scenario counts under, which keys a line of
    /// "Measured figures".
    pub heading: &'static str,
    pub script: Script,
    pub prepare: Prepare,
    /// Why the reference cannot run it hermetically; the port still plays it.
    pub port_only: Option<&'static str>,
}

/// A family: one committed corpus and the scripts it records.
pub struct Family {
    pub name: &'static str,
    /// The corpus, relative to the `vibe-cli` package.
    pub corpus: &'static str,
    pub scenarios: fn() -> Vec<Scenario>,
}

/// What the composer shows once the client is up, on both subjects.
pub const READY: &str = "/help";

/// What the reference shows, beside a spinner, until its session can take
/// input: the masked spinner would make that state look settled.
pub const BOOTING: &str = "Initializing";

/// The terminal every family starts on.
pub const SIZE: (u16, u16) = (120, 40);

/// The model alias every world configures, which the client shows.
const MODEL: &str = "e2e-model";

/// The session title the utility queue answers with.
const TITLE: &str = "Scripted session";

const ANSWER: &str = "Hello from the stand-in.";
const FIRST_ANSWER: &str = "First answer.";
const READ_ANSWER: &str = "The file holds one line.";

pub const STARTUP: Family = Family {
    name: "startup",
    corpus: "tests/tui-blackbox/startup.json",
    scenarios: startup,
};

pub const COMPOSER: Family = Family {
    name: "composer",
    corpus: "tests/tui-blackbox/composer.json",
    scenarios: composer,
};

pub const COMMANDS: Family = Family {
    name: "commands",
    corpus: "tests/tui-blackbox/commands.json",
    scenarios: commands,
};

pub const PATHS: Family = Family {
    name: "paths",
    corpus: "tests/tui-blackbox/paths.json",
    scenarios: paths,
};

pub const TRANSCRIPT: Family = Family {
    name: "transcript",
    corpus: "tests/tui-blackbox/transcript.json",
    scenarios: transcript,
};

pub const FAMILIES: [&Family; 5] = [&STARTUP, &COMPOSER, &COMMANDS, &PATHS, &TRANSCRIPT];

fn script(name: &'static str, authored: Vec<&'static str>, steps: Vec<Step>) -> Script {
    let mut authored = authored;
    authored.extend([MODEL, "e2e-model-v1", "mistral-medium-latest", TITLE]);
    Script {
        name,
        size: SIZE,
        arguments: Vec::new(),
        authored,
        steps,
    }
}

fn titled(world: &World) -> Outcome {
    world.platform().utility(Reply::text(TITLE));
    Ok(())
}

fn answered(world: &World) -> Outcome {
    titled(world)?;
    world.platform().reply(Reply::text(ANSWER));
    Ok(())
}

const STARTUP_HEADING: &str = "tui startup parity:";

fn startup() -> Vec<Scenario> {
    let scenario = |script| Scenario {
        heading: STARTUP_HEADING,
        script,
        prepare: titled,
        port_only: None,
    };
    vec![
        scenario(script(
            "startup/first-screen",
            vec![],
            vec![
                Step::WaitFor(READY),
                Step::WaitGone(BOOTING),
                Step::Snapshot("ready"),
            ],
        )),
        scenario(script(
            "startup/resize",
            vec![],
            vec![
                Step::WaitFor(READY),
                Step::WaitGone(BOOTING),
                Step::Settle,
                Step::Resize(80, 24),
                Step::Snapshot("80x24"),
                Step::Resize(120, 40),
                Step::Snapshot("restored"),
            ],
        )),
        scenario(script(
            "startup/help",
            vec![],
            vec![
                Step::WaitFor(READY),
                Step::WaitGone(BOOTING),
                Step::Type("/help"),
                Step::Submit,
                Step::Snapshot("help"),
                Step::Keys(vec![Key::Esc]),
                Step::Snapshot("dismissed"),
            ],
        )),
        scenario(script(
            "startup/quit",
            vec![],
            vec![
                Step::WaitFor(READY),
                Step::WaitGone(BOOTING),
                Step::Settle,
                Step::Keys(vec![Key::Ctrl('c')]),
                Step::Pause(300),
                Step::Keys(vec![Key::Ctrl('c')]),
                Step::Exit("exited"),
            ],
        )),
    ]
}

const COMPOSER_HEADING: &str = "tui composer parity:";
const COMMANDS_HEADING: &str = "tui commands parity:";
const PATHS_HEADING: &str = "tui paths parity:";

fn seeded(world: &World) -> Outcome {
    titled(world)?;
    world.write("work/notes/plan.md", "# plan\n")?;
    world.write("work/src/lib.rs", "pub fn seeded() {}\n")?;
    world.write("work/src/main.rs", "fn main() {}\n")?;
    Ok(())
}

fn recalled(world: &World) -> Outcome {
    titled(world)?;
    world.platform().reply(Reply::text(FIRST_ANSWER));
    Ok(())
}

fn scenario(heading: &'static str, prepare: Prepare, script: Script) -> Scenario {
    Scenario {
        heading,
        script,
        prepare,
        port_only: None,
    }
}

fn composer() -> Vec<Scenario> {
    vec![
        scenario(
            COMPOSER_HEADING,
            titled,
            script(
                "composer/typing",
                vec!["a draft that is never se"],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("a draft that is never sent"),
                    Step::Snapshot("typed"),
                    Step::Keys(vec![Key::Backspace, Key::Backspace, Key::Left, Key::Left]),
                    Step::Snapshot("edited"),
                ],
            ),
        ),
        scenario(
            COMPOSER_HEADING,
            recalled,
            script(
                "composer/history",
                vec![FIRST_ANSWER],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("first prompt"),
                    Step::Submit,
                    Step::WaitFor(FIRST_ANSWER),
                    Step::Snapshot("answered"),
                    Step::Keys(vec![Key::Up]),
                    Step::Snapshot("recalled"),
                ],
            ),
        ),
        Scenario {
            heading: COMPOSER_HEADING,
            script: script(
                "composer/external-editor",
                vec![],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("draft"),
                    Step::Keys(vec![Key::Ctrl('g')]),
                    Step::Snapshot("editor"),
                ],
            ),
            prepare: titled,
            port_only: Some(
                "the reference starts the editor named by VISUAL or EDITOR on the same terminal, \
                 and no hermetic editor answers it there",
            ),
        },
        Scenario {
            heading: COMPOSER_HEADING,
            script: script(
                "composer/clipboard-image",
                vec![],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Keys(vec![Key::Ctrl('v')]),
                    Step::Snapshot("pasted"),
                ],
            ),
            prepare: titled,
            port_only: Some(
                "the reference reads the system clipboard, which a world cannot seed without \
                 touching the host's",
            ),
        },
    ]
}

/// What the stand-in answers a turn an unknown command may start.
const PROMPTED_ANSWER: &str = "Answered as a prompt.";

fn prompted(world: &World) -> Outcome {
    titled(world)?;
    world.platform().reply(Reply::text(PROMPTED_ANSWER));
    Ok(())
}

fn commands() -> Vec<Scenario> {
    vec![
        scenario(
            COMMANDS_HEADING,
            titled,
            script(
                "commands/popup",
                vec![],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("/"),
                    Step::Snapshot("opened"),
                    Step::Type("co"),
                    Step::Snapshot("filtered"),
                    Step::Keys(vec![Key::Down]),
                    Step::Snapshot("moved"),
                    Step::Keys(vec![Key::Esc]),
                    Step::Snapshot("closed"),
                ],
            ),
        ),
        scenario(
            COMMANDS_HEADING,
            titled,
            script(
                "commands/completion",
                vec![],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("/he"),
                    Step::Snapshot("typed"),
                    Step::Keys(vec![Key::Tab]),
                    Step::Snapshot("completed"),
                ],
            ),
        ),
        scenario(
            COMMANDS_HEADING,
            prompted,
            script(
                "commands/unknown",
                vec!["nosuchcommand", PROMPTED_ANSWER],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("/nosuchcommand"),
                    Step::Submit,
                    Step::Snapshot("submitted"),
                ],
            ),
        ),
    ]
}

fn paths() -> Vec<Scenario> {
    let names = vec!["notes", "plan.md", "src", "lib.rs", "main.rs"];
    vec![
        scenario(
            PATHS_HEADING,
            seeded,
            script(
                "paths/popup",
                names.clone(),
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("@"),
                    Step::Snapshot("opened"),
                    Step::Type("src/"),
                    Step::Snapshot("narrowed"),
                    Step::Keys(vec![Key::Tab]),
                    Step::Snapshot("accepted"),
                ],
            ),
        ),
        scenario(
            PATHS_HEADING,
            seeded,
            script(
                "paths/fuzzy",
                names,
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("look at @pla"),
                    Step::Snapshot("matched"),
                    Step::Keys(vec![Key::Esc]),
                    Step::Snapshot("dismissed"),
                ],
            ),
        ),
    ]
}

const TRANSCRIPT_HEADING: &str = "tui transcript parity:";

fn tool_turn(world: &World) -> Outcome {
    titled(world)?;
    world.write("work/notes.txt", "one seeded line\n")?;
    world.platform().replies([
        Reply::tool_call("read_file", json!({"path": "notes.txt"})),
        Reply::text(READ_ANSWER),
    ]);
    Ok(())
}

fn failed_turn(world: &World) -> Outcome {
    titled(world)?;
    world
        .platform()
        .reply(Reply::status(400).body(r#"{"message":"scripted refusal"}"#));
    Ok(())
}

fn transcript() -> Vec<Scenario> {
    let scenario = |prepare: Prepare, script| Scenario {
        heading: TRANSCRIPT_HEADING,
        script,
        prepare,
        port_only: None,
    };
    vec![
        scenario(
            answered,
            script(
                "transcript/turn",
                vec![ANSWER],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("say hi"),
                    Step::Submit,
                    Step::WaitFor(ANSWER),
                    Step::Snapshot("answered"),
                ],
            ),
        ),
        scenario(
            tool_turn,
            script(
                "transcript/tool-call",
                vec![READ_ANSWER, "notes.txt", "one seeded line"],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("read the notes"),
                    Step::Submit,
                    Step::WaitFor(READ_ANSWER),
                    Step::Snapshot("answered"),
                ],
            ),
        ),
        scenario(
            failed_turn,
            script(
                "transcript/error",
                vec!["scripted refusal"],
                vec![
                    Step::WaitFor(READY),
                    Step::WaitGone(BOOTING),
                    Step::Type("this turn fails"),
                    Step::Submit,
                    Step::Pause(1500),
                    Step::Snapshot("failed"),
                ],
            ),
        ),
    ]
}
