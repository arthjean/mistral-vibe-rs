//! Scenarios, the subject they run against, and the libtest-mimic runner.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use libtest_mimic::{Arguments, Completion, Failed, Trial};

use crate::jail::{self, Jail};
use crate::world::{Binaries, Setup, World};
use crate::{Failure, Outcome, artifacts};

/// Selects the subject: unset or `port` for the shipped binaries, `reference`
/// for the pinned Python reference.
pub const SUBJECT_VARIABLE: &str = "VIBE_E2E_SUBJECT";

/// Which implementation a run spawns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Subject {
    #[default]
    Port,
    Reference,
}

impl fmt::Display for Subject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Port => "port",
            Self::Reference => "reference",
        })
    }
}

type Body = Box<dyn Fn(&mut Ctx) -> Outcome + Send + Sync>;

/// One named trial.
pub struct Scenario {
    name: String,
    body: Arc<Body>,
    known_defect: Option<(String, String)>,
    port_only: Option<String>,
    needs_jail: bool,
    seams: Vec<&'static str>,
}

impl Scenario {
    pub fn new(
        name: impl Into<String>,
        body: impl Fn(&mut Ctx) -> Outcome + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            body: Arc::new(Box::new(body)),
            known_defect: None,
            port_only: None,
            needs_jail: false,
            seams: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The port-only seams this scenario declares it tests.
    pub fn seams(&self) -> &[&'static str] {
        &self.seams
    }

    /// Asserts the reference behavior of an open defect of the register: the
    /// port ignores the trial and prints `id` and `cause`; the reference runs
    /// it.
    #[must_use]
    pub fn known_defect(mut self, id: impl Into<String>, cause: impl Into<String>) -> Self {
        self.known_defect = Some((id.into(), cause.into()));
        self
    }

    /// Skips the trial against the reference, saying why.
    #[must_use]
    pub fn port_only(mut self, reason: impl Into<String>) -> Self {
        self.port_only = Some(reason.into());
        self
    }

    /// Ignores the trial when the binary runs without a network jail.
    #[must_use]
    pub fn needs_jail(mut self) -> Self {
        self.needs_jail = true;
        self
    }

    /// Declares the port-only seam this scenario tests, which lets its source
    /// name the seam ([`crate::seams`]).
    #[must_use]
    pub fn tests_seam(mut self, seam: &'static str) -> Self {
        self.seams.push(seam);
        self
    }
}

struct Shared {
    binaries: Binaries,
    artifacts: Option<PathBuf>,
    jail: Jail,
}

/// What a scenario body receives: its name, its subject, and its worlds.
pub struct Ctx {
    name: String,
    shared: Arc<Shared>,
    worlds: Vec<World>,
}

impl Ctx {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn subject(&self) -> Subject {
        self.shared.binaries.subject
    }

    /// Whether the binary runs inside the network jail.
    pub fn jailed(&self) -> bool {
        self.shared.jail == Jail::Jailed
    }

    /// A world with the default setup: a generic provider on the platform.
    pub fn world(&mut self) -> Outcome<World> {
        self.world_with(Setup::default())
    }

    /// A world built from `setup`, torn down and judged when the trial ends.
    pub fn world_with(&mut self, setup: Setup) -> Outcome<World> {
        let world = World::create(setup, self.shared.binaries.clone())?;
        self.worlds.push(world.clone());
        Ok(world)
    }

    /// Takes the verdict of `world` now and removes it, before the trial ends.
    pub fn close(&mut self, world: World) -> Vec<String> {
        self.worlds.retain(|kept| kept.root() != world.root());
        world.finish()
    }
}

/// Runs `scenario` in a fresh context: the body, then the verdict of every
/// world, then the artifacts when anything failed.
fn execute(name: &str, body: &Body, shared: &Arc<Shared>) -> Result<(), Failed> {
    let mut ctx = Ctx {
        name: name.to_owned(),
        shared: Arc::clone(shared),
        worlds: Vec::new(),
    };
    let artifacts = shared
        .artifacts
        .as_ref()
        .map(|root| root.join(artifacts::directory_name(name)));
    if let Some(directory) = &artifacts {
        let _ = std::fs::remove_dir_all(directory);
    }
    let mut failures = Vec::new();
    match catch_unwind(AssertUnwindSafe(|| body(&mut ctx))) {
        Ok(Ok(())) => {}
        Ok(Err(Failure(message))) => failures.push(message),
        Err(payload) => failures.push(
            payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .map_or_else(
                    || "the scenario panicked".to_owned(),
                    |m| format!("panicked: {m}"),
                ),
        ),
    }
    for world in &ctx.worlds {
        failures.extend(world.finish());
    }
    if failures.is_empty() {
        return Ok(());
    }
    let mut message = failures.join("\n");
    if let Some(directory) = artifacts {
        match artifacts::write(&directory, &ctx.worlds, &failures) {
            Ok(()) => message.push_str(&format!("\nartifacts: {}", directory.display())),
            Err(error) => message.push_str(&format!("\nartifacts not written: {error}")),
        }
    }
    Err(message.into())
}

/// One package's e2e binary.
pub struct Suite {
    package: &'static str,
    binaries: BTreeMap<String, PathBuf>,
    artifacts: Option<PathBuf>,
    reference: Option<fn() -> Result<PathBuf, String>>,
    scenarios: Vec<Scenario>,
}

impl Suite {
    pub fn new(package: &'static str) -> Self {
        Self {
            package,
            binaries: BTreeMap::new(),
            artifacts: None,
            reference: None,
            scenarios: Vec::new(),
        }
    }

    /// A shipped binary scenarios start by `name`.
    #[must_use]
    pub fn binary(mut self, name: &str, path: impl Into<PathBuf>) -> Self {
        self.binaries.insert(name.to_owned(), path.into());
        self
    }

    /// Where failed trials leave artifacts: `<directory>/e2e-artifacts/`.
    #[must_use]
    pub fn artifacts(mut self, directory: impl Into<PathBuf>) -> Self {
        self.artifacts = Some(directory.into().join("e2e-artifacts"));
        self
    }

    /// Resolves the pinned reference checkout, or says why it cannot serve.
    #[must_use]
    pub fn reference(mut self, resolve: fn() -> Result<PathBuf, String>) -> Self {
        self.reference = Some(resolve);
        self
    }

    #[must_use]
    pub fn scenarios(mut self, scenarios: impl IntoIterator<Item = Scenario>) -> Self {
        self.scenarios.extend(scenarios);
        self
    }

    /// The first trial name two scenarios share.
    pub(crate) fn duplicate(&self) -> Option<&str> {
        let mut seen = BTreeSet::new();
        self.scenarios
            .iter()
            .map(Scenario::name)
            .find(|name| !seen.insert(*name))
    }

    /// The subject `variable` selects, resolving the reference when asked.
    pub(crate) fn subject(
        &self,
        variable: Option<&str>,
    ) -> Result<(Subject, Option<PathBuf>), String> {
        match variable.map(str::trim) {
            None | Some("" | "port") => Ok((Subject::Port, None)),
            Some("reference") => {
                let resolve = self.reference.ok_or_else(|| {
                    format!(
                        "{SUBJECT_VARIABLE}=reference: the {} suite declares no reference",
                        self.package
                    )
                })?;
                let root = resolve().map_err(|reason| {
                    format!("{SUBJECT_VARIABLE}=reference needs the pinned reference: {reason}")
                })?;
                Ok((Subject::Reference, Some(root)))
            }
            Some(other) => Err(format!(
                "{SUBJECT_VARIABLE}={other:?} names no subject; use `port` or `reference`"
            )),
        }
    }

    /// One libtest-mimic trial per scenario, with the notices the binary
    /// prints before running them.
    pub(crate) fn trials(
        self,
        subject: Subject,
        reference: Option<PathBuf>,
        jail: Jail,
    ) -> (Vec<Trial>, Vec<String>) {
        let shared = Arc::new(Shared {
            binaries: Binaries {
                subject,
                port: self.binaries,
                reference,
            },
            artifacts: self.artifacts,
            jail: jail.clone(),
        });
        let mut notices = Vec::new();
        let trials = self
            .scenarios
            .into_iter()
            .map(|scenario| {
                let name = scenario.name.clone();
                let ignored = subject == Subject::Port && scenario.known_defect.is_some();
                if let (true, Some((id, cause))) = (ignored, &scenario.known_defect) {
                    notices.push(format!("known defect {id} ({cause}): {name}"));
                }
                let skip = match (&scenario.port_only, subject) {
                    (Some(reason), Subject::Reference) => Some(format!("port only: {reason}")),
                    _ if scenario.needs_jail && jail != Jail::Jailed => {
                        Some("needs the network jail".to_owned())
                    }
                    _ => None,
                };
                let shared = Arc::clone(&shared);
                let body = Arc::clone(&scenario.body);
                Trial::ignorable_test(name.clone(), move || match skip {
                    Some(reason) => Ok(Completion::ignored_with(reason)),
                    None => execute(&name, &body, &shared).map(|()| Completion::Completed),
                })
                .with_ignored_flag(ignored)
            })
            .collect();
        (trials, notices)
    }

    /// Enters the jail, resolves the subject, and runs the trials.
    pub fn run(self) -> ExitCode {
        let arguments = Arguments::from_args();
        let jail = match jail::enter() {
            Ok(jail) => jail,
            Err(message) => {
                eprintln!("e2e: {message}");
                return ExitCode::from(2);
            }
        };
        if cfg!(windows) {
            eprintln!(
                "e2e: Windows support lands in US-034 of tasks/prd-e2e-test-battery.md; running \
                 no trial"
            );
            return libtest_mimic::run(&arguments, Vec::new()).exit_code();
        }
        if let Some(name) = self.duplicate() {
            eprintln!("e2e: two scenarios register the trial name {name:?}");
            return ExitCode::from(2);
        }
        let variable = std::env::var(SUBJECT_VARIABLE).ok();
        let (subject, reference) = match self.subject(variable.as_deref()) {
            Ok(resolved) => resolved,
            Err(message) => {
                eprintln!("e2e: {message}");
                return ExitCode::from(2);
            }
        };
        let package = self.package;
        let (trials, notices) = self.trials(subject, reference, jail);
        if !arguments.list {
            if subject == Subject::Reference {
                eprintln!("e2e: subject: the pinned reference");
            }
            for notice in &notices {
                eprintln!("e2e: {notice}");
            }
        }
        let started = Instant::now();
        let conclusion = libtest_mimic::run(&arguments, trials);
        if !arguments.list {
            summarize(
                package,
                subject,
                &conclusion,
                started.elapsed().as_secs_f64(),
            );
        }
        conclusion.exit_code()
    }
}

/// Appends the run's counts and wall time to the CI job summary.
fn summarize(
    package: &str,
    subject: Subject,
    conclusion: &libtest_mimic::Conclusion,
    seconds: f64,
) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let line = format!(
        "- e2e `{package}` ({subject}): {} passed, {} failed, {} ignored, {} filtered out in {seconds:.1} s\n",
        conclusion.num_passed,
        conclusion.num_failed,
        conclusion.num_ignored,
        conclusion.num_filtered_out
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod suite_tests;
