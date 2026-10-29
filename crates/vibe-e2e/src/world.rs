//! The world a scenario runs in: an isolated home, vibe home, temporary
//! directory, and Git work tree under the system temporary directory, with a
//! platform and an egress sentinel of its own.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::platform::Platform;
use crate::process::{self, ChildRecord, Command};
use crate::sentinel::Sentinel;
use crate::suite::Subject;
use crate::voice::{self, Capture};
use crate::{Failure, Outcome};

/// The variable every world child carries, valued with the world's name, so
/// teardown finds a process the trial left behind.
pub const WORLD_VARIABLE: &str = "VIBE_E2E_WORLD";

/// Names the directory worlds are created in instead of the system temporary
/// directory, which on Windows lies under the user profile.
pub const WORLDS_VARIABLE: &str = "VIBE_E2E_WORLDS";

/// The directory worlds are created in: [`WORLDS_VARIABLE`] when set, the
/// system temporary directory otherwise.
pub(crate) fn worlds_parent() -> PathBuf {
    std::env::var_os(WORLDS_VARIABLE)
        .filter(|value| !value.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// The alias of the model every world configures as active.
pub const MODEL_ALIAS: &str = "e2e-model";

/// The key variable of the generic provider, and the fake key it holds.
pub const PROVIDER_KEY_VARIABLE: &str = "E2E_PROVIDER_KEY";
const FAKE_KEY: &str = "e2e-key";

/// Which backend the world's provider declares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Provider {
    /// An OpenAI-style generic backend keyed by [`PROVIDER_KEY_VARIABLE`].
    #[default]
    Generic,
    /// A Mistral backend keyed by `MISTRAL_API_KEY`, which also turns on the
    /// account, managed configuration, experiment, and telemetry routes.
    Mistral,
}

impl Provider {
    /// The model name the world's `[[models]]` entry declares.
    #[must_use]
    pub fn model_name(self) -> &'static str {
        match self {
            Self::Generic => "e2e-model-v1",
            Self::Mistral => "mistral-medium-latest",
        }
    }
}

/// How a scenario wants its world built.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    provider: Provider,
    api_base: Option<String>,
    config: String,
    env: Vec<(String, String)>,
    voice: Option<Capture>,
}

impl Setup {
    /// A world whose provider is a Mistral backend.
    pub fn mistral() -> Self {
        Self {
            provider: Provider::Mistral,
            ..Self::default()
        }
    }

    /// Points the provider at `api_base` instead of the platform.
    #[must_use]
    pub fn api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = Some(api_base.into());
        self
    }

    /// TOML inserted into `config.toml` after the world's top-level keys and
    /// before its tables.
    #[must_use]
    pub fn config(mut self, toml: impl Into<String>) -> Self {
        self.config.push_str(&toml.into());
        self.config.push('\n');
        self
    }

    /// Declares audio providers on the platform and an ALSA configuration of
    /// the world's own whose default device is `capture` ([`crate::voice`]).
    #[must_use]
    pub fn voice(mut self, capture: Capture) -> Self {
        self.voice = Some(capture);
        self
    }

    /// A variable every child of the world receives.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }
}

/// Which binaries a world's commands resolve to.
#[derive(Debug, Clone, Default)]
pub(crate) struct Binaries {
    pub subject: Subject,
    pub port: BTreeMap<String, PathBuf>,
    pub reference: Option<PathBuf>,
}

impl Binaries {
    pub fn resolve(&self, subject: Subject, name: &str) -> Outcome<PathBuf> {
        match subject {
            Subject::Port => {
                self.port.get(name).cloned().ok_or_else(|| {
                    Failure(format!("the suite declares no port binary named {name}"))
                })
            }
            Subject::Reference => {
                let root = self.reference.as_ref().ok_or_else(|| {
                    Failure("the reference subject was not resolved for this run".to_owned())
                })?;
                let unix = root.join(".venv/bin").join(name);
                let windows = root.join(".venv/Scripts").join(format!("{name}.exe"));
                Ok(if windows.is_file() { windows } else { unix })
            }
        }
    }
}

/// A shared handle to one world; the world is removed when the last handle
/// goes.
#[derive(Clone)]
pub struct World {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    root: PathBuf,
    name: String,
    provider: Provider,
    platform: Platform,
    sentinel: Sentinel,
    env: Vec<(String, String)>,
    binaries: Binaries,
    pub(crate) children: Mutex<Vec<ChildRecord>>,
    finished: AtomicBool,
}

static NEXT_WORLD: AtomicU64 = AtomicU64::new(0);

impl World {
    /// A world whose commands resolve no subject binary, for harness tests.
    pub fn new(setup: Setup) -> Outcome<Self> {
        Self::create(setup, Binaries::default())
    }

    pub(crate) fn create(setup: Setup, binaries: Binaries) -> Outcome<Self> {
        let platform = Platform::start()?;
        let sentinel = Sentinel::start()?;
        let name = format!(
            "vibe-e2e-{}-{}",
            std::process::id(),
            NEXT_WORLD.fetch_add(1, Ordering::SeqCst)
        );
        let parent = dunce_canonical(&worlds_parent())?;
        let root = parent.join(&name);
        fs::create_dir(&root)
            .map_err(|error| Failure(format!("cannot create world {}: {error}", root.display())))?;
        let key = match setup.provider {
            Provider::Generic => PROVIDER_KEY_VARIABLE,
            Provider::Mistral => "MISTRAL_API_KEY",
        };
        let mut env = vec![(key.to_owned(), FAKE_KEY.to_owned())];
        if setup.voice.is_some() {
            let config = root.join(voice::ALSA_CONFIG);
            env.push((
                "ALSA_CONFIG_PATH".to_owned(),
                config.to_string_lossy().into_owned(),
            ));
        }
        env.extend(setup.env.iter().cloned());
        let world = Self {
            inner: Arc::new(Inner {
                root,
                name,
                provider: setup.provider,
                platform,
                sentinel,
                env,
                binaries,
                children: Mutex::new(Vec::new()),
                finished: AtomicBool::new(false),
            }),
        };
        world.check_placement()?;
        for directory in ["home", "vibe-home", "tmp", "work"] {
            fs::create_dir(world.root().join(directory))?;
        }
        world.write(
            "home/.gitconfig",
            "[user]\n\tname = Vibe E2E\n\temail = e2e@example.invalid\n[init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n",
        )?;
        world.write("vibe-home/config.toml", world.config_toml(&setup))?;
        if let Some(capture) = setup.voice {
            let capture_file = world.path(voice::CAPTURE_FILE)?;
            world.write(voice::CAPTURE_FILE, voice::capture_pcm())?;
            world.write(
                voice::ALSA_CONFIG,
                voice::alsa_config(capture, &capture_file.to_string_lossy()),
            )?;
        }
        world.git(&["init", "-q"])?;
        world.git(&["commit", "-q", "--allow-empty", "-m", "e2e world"])?;
        Ok(world)
    }

    /// A world placed under the home or inside a Git work tree would change
    /// config target selection and repository detection.
    fn check_placement(&self) -> Outcome {
        let root = self.root();
        for variable in ["HOME", "USERPROFILE"] {
            let Some(home) = std::env::var_os(variable).filter(|home| !home.is_empty()) else {
                continue;
            };
            let home = dunce_canonical(Path::new(&home)).unwrap_or_else(|_| PathBuf::from(home));
            if root.starts_with(&home) {
                return Err(Failure(format!(
                    "the world {} lies under the home directory {}; set {WORLDS_VARIABLE} to a \
                     directory outside it",
                    root.display(),
                    home.display()
                )));
            }
        }
        if let Some(repository) = root
            .ancestors()
            .skip(1)
            .find(|dir| dir.join(".git").exists())
        {
            return Err(Failure(format!(
                "the world {} lies inside the Git work tree {}",
                root.display(),
                repository.display()
            )));
        }
        Ok(())
    }

    fn config_toml(&self, setup: &Setup) -> String {
        let base = self.platform().base();
        let api_base = setup
            .api_base
            .clone()
            .unwrap_or_else(|| format!("{base}/v1"));
        let model = setup.provider.model_name();
        let provider = match setup.provider {
            Provider::Generic => format!(
                "[[providers]]\nname = \"e2e-provider\"\napi_base = \"{api_base}\"\n\
                 api_key_env_var = \"{PROVIDER_KEY_VARIABLE}\"\napi_style = \"openai\"\n\
                 backend = \"generic\"\n\n[[models]]\nname = \"{model}\"\n\
                 provider = \"e2e-provider\"\nalias = \"{MODEL_ALIAS}\"\ninput_price = 0.0\n\
                 output_price = 0.0\n"
            ),
            Provider::Mistral => format!(
                "[[providers]]\nname = \"mistral\"\napi_base = \"{api_base}\"\n\
                 api_key_env_var = \"MISTRAL_API_KEY\"\nbackend = \"mistral\"\n\n[[models]]\n\
                 name = \"{model}\"\nprovider = \"mistral\"\nalias = \"{MODEL_ALIAS}\"\n\
                 input_price = 0.0\noutput_price = 0.0\n"
            ),
        };
        let (voice_keys, voice_tables) = match setup.voice {
            Some(_) => (
                voice::CONFIG_KEYS,
                voice::config_tables(base, self.key_variable()),
            ),
            None => ("", String::new()),
        };
        format!(
            "active_model = \"{MODEL_ALIAS}\"\nenable_update_checks = false\n\
             console_base_url = \"{base}\"\nvibe_base_url = \"{base}\"\n\
             vibe_code_sessions_base_url = \"{base}\"\n{voice_keys}{}\n[experiments]\n\
             api_host = \"{base}\"\nclient_key = \"sdk-e2e\"\n\n{provider}{voice_tables}",
            setup.config
        )
    }

    fn git(&self, arguments: &[&str]) -> Outcome {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(self.work())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(windows_base())
            .output()
            .map_err(|error| Failure(format!("cannot run git: {error}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Failure(format!(
                "git {} failed in the world: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The world's name, which every child carries in [`WORLD_VARIABLE`].
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    pub fn vibe_home(&self) -> PathBuf {
        self.root().join("vibe-home")
    }

    pub fn work(&self) -> PathBuf {
        self.root().join("work")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root().join("tmp")
    }

    pub fn platform(&self) -> &Platform {
        &self.inner.platform
    }

    pub fn provider(&self) -> Provider {
        self.inner.provider
    }

    /// The variable the provider's key is read from.
    pub fn key_variable(&self) -> &'static str {
        match self.inner.provider {
            Provider::Generic => PROVIDER_KEY_VARIABLE,
            Provider::Mistral => "MISTRAL_API_KEY",
        }
    }

    /// The subject this run spawns for a binary name.
    pub fn subject(&self) -> Subject {
        self.inner.binaries.subject
    }

    /// The environment every child starts from, built from empty.
    pub fn env(&self) -> Vec<(String, String)> {
        let proxy = self.inner.sentinel.base().to_owned();
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        let mut env: Vec<(String, String)> = vec![
            (
                "PATH".to_owned(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned()),
            ),
            ("HOME".to_owned(), path(self.home())),
            ("VIBE_HOME".to_owned(), path(self.vibe_home())),
            ("TMPDIR".to_owned(), path(self.tmp())),
            ("LANG".to_owned(), "C.UTF-8".to_owned()),
            ("TERM".to_owned(), "dumb".to_owned()),
            (
                "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                "unix:path=/nonexistent".to_owned(),
            ),
        ];
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
            env.push((name.to_owned(), proxy.clone()));
            env.push((name.to_ascii_lowercase(), proxy.clone()));
        }
        env.push(("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()));
        env.push(("no_proxy".to_owned(), "127.0.0.1,localhost".to_owned()));
        env.push((WORLD_VARIABLE.to_owned(), self.name().to_owned()));
        // Windows resolves the home from the profile directory, not HOME, and
        // its credential store is the runner's own: the D-Bus address above
        // keeps Linux children off the keyring, the reference's test hook
        // keeps Windows children off it.
        if cfg!(windows) {
            env.push(("USERPROFILE".to_owned(), path(self.home())));
            env.push(("VIBE_TEST_DISABLE_KEYRING".to_owned(), "1".to_owned()));
        }
        env.extend(windows_base().map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned())));
        env.extend(self.inner.env.iter().cloned());
        env
    }

    /// The path `relative` names inside the world, refused when it resolves
    /// outside the root. Parent directories are created.
    pub fn path(&self, relative: impl AsRef<Path>) -> Outcome<PathBuf> {
        let relative = relative.as_ref();
        let root = self.root();
        let refuse = || {
            Failure(format!(
                "seed path {} resolves outside the world root {}",
                relative.display(),
                root.display()
            ))
        };
        let relative = if relative.is_absolute() {
            relative.strip_prefix(root).map_err(|_| refuse())?
        } else {
            relative
        };
        let mut resolved = root.to_path_buf();
        for component in relative.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !resolved.pop() || !resolved.starts_with(root) {
                        return Err(refuse());
                    }
                }
                Component::RootDir | Component::Prefix(_) => return Err(refuse()),
            }
        }
        if !resolved.starts_with(root) {
            return Err(refuse());
        }
        if let Some(parent) = resolved.parent() {
            // Only an existing component can be a symlink, so the deepest one
            // decides where the missing ones would be created.
            let existing = parent.ancestors().find(|dir| dir.exists()).unwrap_or(root);
            if !dunce_canonical(existing)?.starts_with(root) {
                return Err(refuse());
            }
            fs::create_dir_all(parent)?;
        }
        if resolved.is_symlink() && !dunce_canonical(&resolved)?.starts_with(root) {
            return Err(refuse());
        }
        Ok(resolved)
    }

    /// Writes a seed file inside the world.
    pub fn write(&self, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Outcome {
        let path = self.path(relative)?;
        fs::write(&path, contents)
            .map_err(|error| Failure(format!("cannot write {}: {error}", path.display())))
    }

    /// A command for the subject's binary named `name` (`vibe`, `vibe-acp`).
    pub fn command(&self, name: &str) -> Outcome<Command> {
        self.command_for(self.subject(), name)
    }

    /// A command for `name` as `subject` ships it, whatever this run's subject.
    pub fn command_for(&self, subject: Subject, name: &str) -> Outcome<Command> {
        let program = self.inner.binaries.resolve(subject, name)?;
        Ok(Command::new(self.clone(), program, name.to_owned()))
    }

    /// The path of the subject's binary named `name`, for a scenario that
    /// starts it through a shell.
    pub fn binary(&self, name: &str) -> Outcome<PathBuf> {
        self.inner.binaries.resolve(self.subject(), name)
    }

    /// A command for any program, found on `PATH` when `program` is a bare
    /// name, with the world's environment.
    pub fn program(&self, program: impl AsRef<OsStr>) -> Command {
        let program = PathBuf::from(program.as_ref());
        let label = program.file_name().map_or_else(
            || "program".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        Command::new(self.clone(), program, label)
    }

    /// Every `host:port` the sentinel recorded.
    pub fn egress(&self) -> Vec<String> {
        self.inner.sentinel.hosts()
    }

    /// Waits until the sentinel records a host, and returns what it recorded.
    pub fn await_egress(&self, deadline: std::time::Duration) -> Outcome<Vec<String>> {
        let until = std::time::Instant::now() + crate::scaled(deadline);
        loop {
            let hosts = self.egress();
            if !hosts.is_empty() {
                return Ok(hosts);
            }
            if std::time::Instant::now() >= until {
                return Err(Failure(format!(
                    "the sentinel recorded no host within {:?}",
                    crate::scaled(deadline)
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Forgets recorded egress, after a self-test provoked it on purpose.
    pub fn clear_egress(&self) {
        self.inner.sentinel.clear();
    }

    pub(crate) fn register(&self, record: ChildRecord) {
        self.inner
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }

    pub(crate) fn records(&self) -> Vec<ChildRecord> {
        self.inner
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The trial's verdict on this world: processes still carrying its marker
    /// (killed), platform failures, and recorded egress. A second call returns
    /// nothing, so a self-test can take the verdict before the trial does.
    pub fn finish(&self) -> Vec<String> {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        let mut failures = process::reap_survivors(self.name());
        failures.extend(self.platform().failures());
        failures.extend(
            self.egress()
                .into_iter()
                .map(|host| format!("egress to {host} recorded by the sentinel")),
        );
        failures
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            let _ = process::reap_survivors(&self.name);
        }
        remove_tree(&self.root);
    }
}

/// Restores owner write access below `root`, then removes it.
pub(crate) fn remove_tree(root: &Path) {
    make_writable(root);
    let _ = fs::remove_dir_all(root);
}

fn make_writable(directory: &Path) {
    let Ok(metadata) = fs::symlink_metadata(directory) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o700);
    }
    #[cfg(not(unix))]
    clear_read_only(&mut permissions);
    let _ = fs::set_permissions(directory, permissions);
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

/// Clears the read-only attribute, which is all Windows has for mode bits.
#[cfg(not(unix))]
#[allow(
    clippy::permissions_set_readonly_false,
    reason = "only compiled where there are no Unix mode bits to widen"
)]
fn clear_read_only(permissions: &mut fs::Permissions) {
    permissions.set_readonly(false);
}

/// `path` canonicalized without the `\\?\` prefix Windows adds.
fn dunce_canonical(path: &Path) -> Outcome<PathBuf> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| Failure(format!("cannot resolve {}: {error}", path.display())))?;
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
}

/// What Windows itself reads in any child environment (see
/// `crates/vibe-cli/tests/support/mod.rs`); empty elsewhere.
fn windows_base() -> impl Iterator<Item = (&'static str, std::ffi::OsString)> {
    const BASE: [&str; 5] = ["SystemRoot", "SystemDrive", "windir", "COMSPEC", "PATHEXT"];
    BASE.into_iter()
        .filter(|_| cfg!(windows))
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
}

#[cfg(test)]
mod world_tests;
