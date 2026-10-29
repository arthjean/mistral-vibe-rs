//! US-029: `vibe-acp --setup` on a pseudo-terminal, the sign-in an editor
//! starts when `session/new` asks for authentication.
//!
//! The reference's setup is its Textual onboarding (`run_onboarding`, reached
//! from vibe/acp/entrypoint.py), which is not driven, so these trials run
//! against the port alone and read the expectation from the reference source:
//! the key lands in `VIBE_HOME/.env`, created 0600
//! (`_save_api_key_to_env_file` in vibe/setup/auth/api_key_persistence.py),
//! and setup exits.

use std::time::Duration;

use vibe_e2e::{Ctx, Key, Outcome, Scenario, Setup, ensure};

const STEP: Duration = Duration::from_secs(20);

const TEXTUAL: &str = "the reference's Textual onboarding is not driven; the expectation is read \
                       from vibe/setup/auth/api_key_persistence.py";

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new("acp::setup::a_typed_key_is_saved", |ctx| setup(ctx, false))
            .port_only(TEXTUAL),
        Scenario::new("acp::setup::a_pasted_key_is_saved", |ctx| setup(ctx, true))
            .port_only(TEXTUAL)
            .known_defect(
                "E2E-D50",
                "onboarding turns bracketed paste on and drops the pasted key",
            ),
    ]
}

fn setup(ctx: &mut Ctx, paste: bool) -> Outcome {
    let world = ctx.world_with(Setup::mistral())?;
    let mut tui = world
        .command("vibe-acp")?
        .arg("--setup")
        .env_remove("MISTRAL_API_KEY")
        .spawn_tui(100, 30)?;
    tui.wait_text("Press Enter to start", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("Pick the colors you want to work in", STEP)?;
    tui.send(&[Key::Enter])?;
    tui.wait_text("Paste an API key", STEP)?;
    tui.send(&[Key::Down, Key::Enter])?;
    tui.wait_text("Paste API key", STEP)?;
    let key = "acp-setup-key".to_owned();
    tui.send(&[if paste {
        Key::Paste(key)
    } else {
        Key::Text(key)
    }])?;
    tui.send(&[Key::Enter])?;
    let status = tui.wait_exit(STEP)?;
    let screen = tui.screen();
    ensure(status.success, || {
        format!("setup ended with {status}:\n{}", screen.text())
    })?;
    let env = world.vibe_home().join(".env");
    let text = std::fs::read_to_string(&env)
        .map_err(|error| format!("no {}: {error}\n{}", env.display(), screen.text()))?;
    ensure(
        text.lines()
            .any(|line| line.starts_with("MISTRAL_API_KEY=") && line.contains("acp-setup-key")),
        || format!(".env holds {text:?}"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env)?.permissions().mode() & 0o777;
        ensure(mode == 0o600, || format!(".env has mode {mode:o}"))?;
    }
    ensure(world.platform().chat_requests().is_empty(), || {
        "setup sent a chat request".to_owned()
    })
}
