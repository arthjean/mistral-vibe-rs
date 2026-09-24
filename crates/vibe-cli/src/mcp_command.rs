//! `vibe mcp`: the sub-command surface, its help renders and its error grammar.
//!
//! Reference `vibe/cli/mcp_command.py`. The reference builds three argparse
//! parsers, dispatches on the sub-command name, and funnels every post-parse
//! failure back through `parser.error`, which prints the usage line, one
//! `{prog}: error: {message}` line, and exits 2. This module reproduces that
//! shape over a clap declaration: clap owns the argument surface, so
//! `crate::cli_surface_parity_tests` can read it back and compare it against
//! the recorded argparse actions, while the usage line, the help render and the
//! error grammar are written here because clap spells all three differently.
//!
//! The intercept runs before the top-level parser (`crates/vibe-cli/src/main.rs`),
//! which is where the reference puts its own (`vibe/cli/entrypoint.py:258`), so
//! `mcp` is never read as a positional prompt.
//!
//! Every persistence path opens a user-scope store, matching the user-only
//! harness the reference builds for this command (`mcp_command.py:383-390`):
//! `vibe mcp remove` never edits a project configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction, ArgMatches, Command};
use toml::Table;
use url::Url;
use vibe_app_server::resources::{McpAuthBackend, production_mcp_adapters};
use vibe_core::auth::{
    KeyringBackend, NativeKeyringBackend, delete_mcp_oauth_credential, open_system_browser,
};
use vibe_core::config::mcp::normalize_mcp_server_url_with;
use vibe_core::config::{ConfigError, ConfigPaths, ConfigSource, LayeredConfig};
use vibe_core::mcp::{
    DEFAULT_MCP_API_KEY_FORMAT, DEFAULT_MCP_API_KEY_HEADER, DEFAULT_MCP_STARTUP_TIMEOUT_MS,
    DEFAULT_MCP_TOOL_TIMEOUT_MS, McpAuthConfig, McpOAuthConfig, McpServerConfig, McpStaticAuth,
    McpTransportConfig,
};

/// The program name every usage line and every error line carries.
const PROG: &str = "vibe mcp";

/// The exit code an argument failure carries, which is argparse's own.
const USAGE_EXIT: u8 = 2;

/// The transport an invocation that names none asks for.
const DEFAULT_TRANSPORT: &str = "streamable-http";

/// argparse's help column: two past the widest invocation, capped.
const MAX_HELP_POSITION: usize = 24;

/// Whether `arguments` is a `vibe mcp` invocation, which is decided before the
/// interactive parser sees them because `mcp` is not a prompt.
#[must_use]
pub fn intercepts(arguments: &[String]) -> bool {
    arguments.first().map(String::as_str) == Some("mcp")
}

// --------------------------------------------------------------------------
// The declaration
// --------------------------------------------------------------------------

/// The `vibe mcp` parser, its two sub-commands and their arguments.
///
/// The value names are the ones argparse renders: a declared `metavar` where
/// the reference declares one, the choice set where it declares choices, and
/// the uppercased destination otherwise.
#[must_use]
pub(crate) fn declaration() -> Command {
    let mut command = Command::new(PROG)
        // The description is the block a help render prints under its usage
        // line. Only the root parser declares one: the reference passes
        // `description` to `ArgumentParser` and only `help` to `add_parser`,
        // so a sub-command's blurb is listed by its parent and never printed
        // again as its own description.
        .long_about("Configure the MCP servers a session may reach.")
        .disable_help_subcommand(true)
        // argparse keeps the last occurrence of an option given twice.
        .args_override_self(true)
        .subcommand(add_declaration())
        .subcommand(remove_declaration());
    command.build();
    command
}

/// The help flag argparse adds before anything else.
///
/// clap appends its own at the end of the list, and a help render lists its
/// arguments in declaration order, so a sub-command that wants `-h` first has
/// to declare it itself.
fn help_argument() -> Arg {
    Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::Help)
        .help("Print this help text and exit.")
}

fn add_declaration() -> Command {
    Command::new("add")
        .about("Store an MCP server in the user configuration.")
        .disable_help_flag(true)
        .arg(help_argument())
        .arg(
            Arg::new("name")
                .value_name("NAME")
                .required(true)
                .help("Name the server is configured under."),
        )
        .arg(
            Arg::new("transport")
                .long("transport")
                .value_name("{http,streamable-http,stdio}")
                .value_parser(["http", "streamable-http", "stdio"])
                .default_value(DEFAULT_TRANSPORT)
                .help("Wire protocol the server speaks."),
        )
        .arg(
            Arg::new("url")
                .long("url")
                .value_name("URL")
                .help("Endpoint a remote transport connects to."),
        )
        .arg(
            Arg::new("command")
                .long("command")
                .value_name("COMMAND")
                .help("Executable a stdio server is launched from."),
        )
        .arg(
            Arg::new("arg")
                .long("arg")
                .value_name("VALUE")
                .action(ArgAction::Append)
                .help("Argument passed to the stdio command; repeatable."),
        )
        .arg(
            Arg::new("env")
                .long("env")
                .value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Variable set for the stdio command; repeatable."),
        )
        .arg(
            Arg::new("header")
                .long("header")
                .value_name("NAME=VALUE")
                .action(ArgAction::Append)
                .help("Header sent with every request; repeatable."),
        )
        .arg(
            Arg::new("api_key_env")
                .long("api-key-env")
                .visible_alias("bearer-token-env-var")
                .value_name("VAR")
                .help("Variable holding the API key."),
        )
        .arg(
            Arg::new("api_key_header")
                .long("api-key-header")
                .value_name("HEADER")
                .help("Header the API key is sent in."),
        )
        .arg(
            Arg::new("api_key_format")
                .long("api-key-format")
                .value_name("FORMAT")
                .help("Template the API key is formatted with."),
        )
        .arg(
            Arg::new("no_login")
                .long("no-login")
                .action(ArgAction::SetTrue)
                .help("Store the server without an OAuth login."),
        )
        .arg(
            Arg::new("allow_insecure_http")
                .long("allow-insecure-http")
                .action(ArgAction::SetTrue)
                .help(
                    "Accept a plain http:// URL on a host other than this machine. Nothing \
                     sent to it, credentials included, is encrypted.",
                ),
        )
        .arg(
            Arg::new("startup_timeout_sec")
                .long("startup-timeout-sec")
                .value_name("SECONDS")
                .value_parser(crate::argv::python_float)
                .help("Seconds allowed for the server to start."),
        )
        .arg(
            Arg::new("tool_timeout_sec")
                .long("tool-timeout-sec")
                .value_name("SECONDS")
                .value_parser(crate::argv::python_float)
                .help("Seconds allowed for one tool call."),
        )
}

fn remove_declaration() -> Command {
    Command::new("remove")
        .about("Drop an MCP server from the user configuration.")
        .disable_help_flag(true)
        .arg(help_argument())
        .arg(
            Arg::new("name")
                .value_name("NAME")
                .required(true)
                .help("Name of the server to drop."),
        )
}

// --------------------------------------------------------------------------
// The OAuth login a remote add performs
// --------------------------------------------------------------------------

/// The session identity the login is filed under.
///
/// The backend keys a pending login by session and resource, so `begin` and
/// `finish` have to name the same session; a command that runs one login and
/// exits needs only the one name.
const LOGIN_SESSION: &str = "vibe-mcp-add";

/// How long a poll waits past the backend's own login timeout before giving up
/// on it answering at all.
const LOGIN_POLL_LIMIT: Duration = Duration::from_secs(310);

/// How often the poll asks whether the authorization server came back.
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) type LoginFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + 'a>>;

/// The browser login a remote `add` runs, injected for the same reason the
/// credential store is: a replay that drove the real one would open a browser
/// and reach a live authorization server.
pub(crate) trait McpOAuthLogin {
    /// Starts a login and answers with the URL that authorizes it.
    fn begin<'a>(&'a self, config: &'a McpServerConfig) -> LoginFuture<'a, String>;

    /// Waits for the login [`Self::begin`] started to finish.
    fn finish<'a>(&'a self, config: &'a McpServerConfig) -> LoginFuture<'a, ()>;

    /// Hands `url` to the host's browser. A host with no launcher is reported
    /// and not fatal: the URL was printed first, so the login can still be
    /// completed by hand.
    fn open(&self, url: &str) -> Result<(), String>;
}

/// The login the installed binary runs, over the app server's OAuth backend.
#[derive(Default)]
struct ProcessOAuthLogin {
    /// One backend for both halves of a login: it holds the pending exchange
    /// in memory, so a second instance would not recognize what the first
    /// started.
    backend: OnceLock<Arc<dyn McpAuthBackend>>,
}

impl ProcessOAuthLogin {
    fn backend(&self) -> Result<Arc<dyn McpAuthBackend>, String> {
        if let Some(backend) = self.backend.get() {
            return Ok(backend.clone());
        }
        let (_, auth) = production_mcp_adapters(None).map_err(|error| error.to_string())?;
        Ok(self.backend.get_or_init(|| auth).clone())
    }
}

impl McpOAuthLogin for ProcessOAuthLogin {
    fn begin<'a>(&'a self, config: &'a McpServerConfig) -> LoginFuture<'a, String> {
        Box::pin(async move {
            let backend = self.backend()?;
            backend
                .login(LOGIN_SESSION, config)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn finish<'a>(&'a self, config: &'a McpServerConfig) -> LoginFuture<'a, ()> {
        Box::pin(async move {
            let backend = self.backend()?;
            // The backend answers a pending login without blocking, so the
            // wait is this loop rather than a call that parks. It also carries
            // its own timeout, and the limit here only bounds a backend that
            // stops answering at all.
            let deadline = Instant::now() + LOGIN_POLL_LIMIT;
            loop {
                match backend.complete(LOGIN_SESSION, config).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => return Err(error.to_string()),
                }
                if Instant::now() >= deadline {
                    return Err("the authorization server did not answer".to_owned());
                }
                tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
            }
        })
    }

    fn open(&self, url: &str) -> Result<(), String> {
        if open_system_browser(url) {
            Ok(())
        } else {
            Err("this host has no browser launcher".to_owned())
        }
    }
}

// --------------------------------------------------------------------------
// The environment a run resolves against
// --------------------------------------------------------------------------

/// Everything `vibe mcp` touches outside its own argument vector.
///
/// Both halves are injected rather than resolved inside the command, because a
/// replay that drove the real ones would read, and on a hit rewrite, the
/// developer's own configuration and credential store.
pub struct McpEnvironment {
    vibe_home: PathBuf,
    working_directory: PathBuf,
    keyring: Box<dyn KeyringBackend>,
    login: Box<dyn McpOAuthLogin>,
}

impl McpEnvironment {
    /// The environment the installed binary runs against.
    #[must_use]
    pub fn from_process() -> Self {
        let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let vibe_home =
            crate::tui::startup::workspace_paths_for(None, &working_directory).vibe_home;
        Self {
            vibe_home,
            working_directory,
            keyring: Box::new(NativeKeyringBackend::new()),
            login: Box::new(ProcessOAuthLogin::default()),
        }
    }

    /// An environment over a named home and credential store, for the replay
    /// and the unit tests.
    ///
    /// Only a test builds one: the binary always resolves the real home, and a
    /// replay that did the same would read, and on a hit rewrite, the
    /// developer's own configuration.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_home(
        vibe_home: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        keyring: Box<dyn KeyringBackend>,
        login: Box<dyn McpOAuthLogin>,
    ) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            working_directory: working_directory.into(),
            keyring,
            login,
        }
    }

    /// The user-scope store every persistence path here writes through.
    fn store(&self) -> LayeredConfig {
        LayeredConfig::new(
            ConfigPaths {
                vibe_home: self.vibe_home.clone(),
                working_directory: self.working_directory.clone(),
            },
            Table::new(),
        )
        .with_sources(BTreeSet::from([ConfigSource::User]))
    }
}

// --------------------------------------------------------------------------
// The run
// --------------------------------------------------------------------------

/// Runs `vibe mcp <subcommand>` and answers with the process exit code.
///
/// The dispatch is argparse's: the first token names the sub-command, a token
/// the parser does not declare is a choice failure against `mcp_command`, and
/// anything the sub-command could not place is reported by the root parser.
#[must_use]
pub async fn run(
    arguments: &[String],
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let root = declaration();
    match arguments.split_first() {
        None => print_help(&root, stdout),
        // argparse resolves an abbreviated `--help` too, and the root parser
        // declares no other long option for a prefix to be ambiguous with.
        Some((first, _))
            if first == "-h" || (first.len() > 2 && "--help".starts_with(first.as_str())) =>
        {
            print_help(&root, stdout)
        }
        Some((first, rest)) if root.find_subcommand(first.as_str()).is_some() => {
            let Some(sub) = root.find_subcommand(first.as_str()).cloned() else {
                return USAGE_EXIT;
            };
            dispatch(&root, sub, first, rest, environment, stdout, stderr).await
        }
        Some((first, _)) if first.starts_with('-') => {
            fail(&root, PROG, &unrecognized(first), stderr)
        }
        Some((first, _)) => fail(
            &root,
            PROG,
            &format!(
                "argument mcp_command: invalid choice: '{first}' (choose from {})",
                subcommand_names(&root).join(", ")
            ),
            stderr,
        ),
    }
}

async fn dispatch(
    root: &Command,
    sub: Command,
    name: &str,
    rest: &[String],
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let prog = format!("{PROG} {name}");
    let argv = std::iter::once(prog.clone())
        .chain(rest.iter().cloned())
        .map(std::ffi::OsString::from)
        .collect();
    // The sub-command reads its tokens the way argparse does, abbreviations
    // and hyphen-led values included, before clap sees them.
    let reading = crate::argv::reading::read(&sub, argv, &[]);
    let parsed = sub.clone().try_get_matches_from(reading.argv);
    if let (Ok(_), Some(message)) = (&parsed, &reading.refusal) {
        return fail(&sub, &prog, message, stderr);
    }
    let matches = match parsed {
        Ok(matches) => matches,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return print_help(&sub, stdout);
        }
        Err(error) => {
            let (scope, message) = translate(&error);
            return match scope {
                Scope::Root => fail(root, PROG, &message, stderr),
                Scope::Sub => fail(&sub, &prog, &message, stderr),
            };
        }
    };
    match name {
        "remove" => {
            let requested = matches
                .get_one::<String>("name")
                .cloned()
                .unwrap_or_default();
            match remove(&requested, environment) {
                Ok(message) => write_line(stdout, &message),
                Err(message) => fail(&sub, &prog, &message, stderr),
            }
        }
        // A post-parse failure is reported by the root parser, not by the one
        // that was running: the reference funnels every one of them back
        // through `parser.error` on the parser it built first.
        _ => match add(&matches, environment, stdout, stderr).await {
            Ok(code) => code,
            Err(message) => fail(root, PROG, &message, stderr),
        },
    }
}

/// One `add` invocation, reduced to the entry it asks for and whether it also
/// asks for a login.
struct AddCommand {
    config: McpServerConfig,
    login: bool,
}

/// Stores the requested server, then logs in when the entry asks for OAuth and
/// the invocation did not decline it.
///
/// Reference `_add_mcp_server`. The entry is on disk before the browser opens,
/// which is what makes a login failure recoverable: the server is configured,
/// and `/mcp login` can finish the exchange later.
async fn add(
    matches: &ArgMatches,
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<u8, String> {
    let command = parse_add(matches)?;
    let addition = environment
        .store()
        .persist_mcp_server(&command.config)
        .map_err(mcp_failure)?;
    let message = if addition.created {
        format!("Added MCP server `{}`.", addition.server.alias)
    } else {
        format!(
            "MCP server `{}` is already configured.",
            addition.server.alias
        )
    };
    if !matches!(addition.server.auth, McpAuthConfig::Oauth(_)) {
        return Ok(write_line(stdout, &message));
    }
    if !command.login {
        return Ok(write_line(
            stdout,
            &format!(
                "{message}\nRun `/mcp login {}` to authenticate.",
                addition.server.alias
            ),
        ));
    }
    // With a login to run, the reference reports the entry as soon as it is
    // written rather than at the end, so the narration reaches the terminal
    // before the wait does.
    let _ = writeln!(stdout, "{message}");
    match authenticate(&addition.server, environment, stdout, stderr).await {
        Ok(()) => Ok(write_line(stdout, "OAuth login succeeded.")),
        Err(error) => {
            let _ = writeln!(
                stderr,
                "{PROG} add: the OAuth login did not finish: {error}"
            );
            let _ = writeln!(
                stderr,
                "Run `/mcp login {}` to authenticate.",
                addition.server.alias
            );
            Ok(1)
        }
    }
}

/// Narrates the authorization URL and waits for the exchange behind it.
///
/// A host with no browser launcher is reported and not fatal: the URL was
/// printed first, so the login can still be completed by hand.
async fn authenticate(
    config: &McpServerConfig,
    environment: &McpEnvironment,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), String> {
    let url = environment.login.begin(config).await?;
    let _ = writeln!(stdout, "Authorize the server at this URL:\n\n  {url}");
    if let Err(reason) = environment.login.open(&url) {
        let _ = writeln!(stderr, "The browser did not open: {reason}");
    }
    environment.login.finish(config).await
}

/// Reads the argument vector as the entry it describes.
///
/// Reference `_parse_add_args`: the transport picks the branch, and each branch
/// refuses the other's flags before it reads its own.
fn parse_add(matches: &ArgMatches) -> Result<AddCommand, String> {
    let alias = vibe_core::config::mcp::normalize_mcp_server_name(
        matches.get_one::<String>("name").map_or("", String::as_str),
    );
    let transport = matches
        .get_one::<String>("transport")
        .map_or(DEFAULT_TRANSPORT, String::as_str);
    if transport == "stdio" {
        return Ok(AddCommand {
            config: stdio_add(matches, alias)?,
            login: false,
        });
    }
    remote_add(matches, alias, transport)
}

fn stdio_add(matches: &ArgMatches, alias: String) -> Result<McpServerConfig, String> {
    let remote_only = named(&[
        ("--url", text(matches, "url").is_some()),
        ("--header", !texts(matches, "header").is_empty()),
        ("--api-key-env", text(matches, "api_key_env").is_some()),
        (
            "--api-key-header",
            text(matches, "api_key_header").is_some(),
        ),
        (
            "--api-key-format",
            text(matches, "api_key_format").is_some(),
        ),
        ("--no-login", matches.get_flag("no_login")),
        (
            "--allow-insecure-http",
            matches.get_flag("allow_insecure_http"),
        ),
    ]);
    if !remote_only.is_empty() {
        return Err(format!("--transport stdio does not accept {remote_only}."));
    }
    let Some(command) = text(matches, "command") else {
        return Err("--transport stdio needs --command.".to_owned());
    };
    Ok(server(
        alias,
        // The launch directory is left for a reader to resolve, as the
        // reference stores no `cwd` for an entry the command line built.
        McpTransportConfig::Stdio {
            command: command.to_owned(),
            arguments: texts(matches, "arg")
                .into_iter()
                .map(str::to_owned)
                .collect(),
            environment: pairs(&texts(matches, "env"), "--env", Case::Sensitive)?,
            working_directory: None,
        },
        McpAuthConfig::default(),
        matches,
    ))
}

fn remote_add(matches: &ArgMatches, alias: String, transport: &str) -> Result<AddCommand, String> {
    let stdio_only = named(&[
        ("--command", text(matches, "command").is_some()),
        ("--arg", !texts(matches, "arg").is_empty()),
        ("--env", !texts(matches, "env").is_empty()),
    ]);
    if !stdio_only.is_empty() {
        return Err(format!("only --transport stdio accepts {stdio_only}."));
    }
    let Some(requested) = text(matches, "url") else {
        return Err("--url is what an http or streamable-http server is reached at.".to_owned());
    };
    let headers = pairs(&texts(matches, "header"), "--header", Case::Insensitive)?;
    let api_key_env = text(matches, "api_key_env");
    let api_key_header = text(matches, "api_key_header");
    let api_key_format = text(matches, "api_key_format");
    // Naming any part of a static scheme selects it, which is what makes the
    // OAuth default the answer to an invocation that names none of them.
    let statics = !headers.is_empty()
        || api_key_env.is_some()
        || api_key_header.is_some()
        || api_key_format.is_some();
    let no_login = matches.get_flag("no_login");
    if statics && no_login {
        return Err(
            "--no-login asks for OAuth, which the static authentication options replace."
                .to_owned(),
        );
    }
    let auth = if statics {
        McpAuthConfig::Static(static_auth(
            &headers,
            api_key_env,
            api_key_header,
            api_key_format,
        )?)
    } else {
        McpAuthConfig::Oauth(McpOAuthConfig::default())
    };
    // Reference `_parse_mcp_server_url`: the flag lifts only the plaintext
    // refusal, and only for the URL this invocation stores.
    let normalized =
        normalize_mcp_server_url_with(requested, matches.get_flag("allow_insecure_http"))
            .map_err(mcp_failure)?;
    let url = Url::parse(&normalized)
        .map_err(|_| "--url is not an address a server can be reached at.".to_owned())?;
    let transport = if transport == "http" {
        McpTransportConfig::Http { url, headers }
    } else {
        McpTransportConfig::StreamableHttp { url, headers }
    };
    let login = matches!(auth, McpAuthConfig::Oauth(_)) && !no_login;
    Ok(AddCommand {
        config: server(alias, transport, auth, matches),
        login,
    })
}

/// Reference `_build_static_auth`: the two carrier options describe a key the
/// environment holds, so neither means anything without the variable that
/// holds it, and the carrier a `--header` already spells is a conflict rather
/// than an override.
fn static_auth(
    headers: &BTreeMap<String, String>,
    api_key_env: Option<&str>,
    api_key_header: Option<&str>,
    api_key_format: Option<&str>,
) -> Result<McpStaticAuth, String> {
    if api_key_header.is_some() && api_key_env.is_none() {
        return Err("--api-key-header needs --api-key-env.".to_owned());
    }
    if api_key_format.is_some() && api_key_env.is_none() {
        return Err("--api-key-format needs --api-key-env.".to_owned());
    }
    let carrier = api_key_header.unwrap_or(DEFAULT_MCP_API_KEY_HEADER);
    if api_key_env.is_some()
        && headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case(carrier))
    {
        return Err(format!(
            "--header already defines the API key header `{carrier}`."
        ));
    }
    Ok(McpStaticAuth {
        api_key_env: api_key_env.unwrap_or_default().to_owned(),
        api_key_header: carrier.to_owned(),
        api_key_format: api_key_format
            .unwrap_or(DEFAULT_MCP_API_KEY_FORMAT)
            .to_owned(),
    })
}

/// Whether a repeated collection reads two names as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Sensitive,
    Insensitive,
}

/// One repeated `NAME=VALUE` option, in the grammar the reference parses:
/// the first `=` separates, both halves are trimmed, and a name the collection
/// already carries is refused rather than overwritten.
fn pairs(values: &[&str], flag: &str, case: Case) -> Result<BTreeMap<String, String>, String> {
    let mut parsed = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let Some((name, value)) = value.split_once('=') else {
            return Err(format!("{flag} values are spelled NAME=VALUE."));
        };
        let name = name.trim();
        let key = match case {
            Case::Sensitive => name.to_owned(),
            Case::Insensitive => name.to_lowercase(),
        };
        if !seen.insert(key) {
            return Err(format!("{flag} names `{name}` twice."));
        }
        parsed.insert(name.to_owned(), value.trim().to_owned());
    }
    Ok(parsed)
}

fn server(
    alias: String,
    transport: McpTransportConfig,
    auth: McpAuthConfig,
    matches: &ArgMatches,
) -> McpServerConfig {
    McpServerConfig {
        alias,
        transport,
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: milliseconds(
            matches,
            "startup_timeout_sec",
            DEFAULT_MCP_STARTUP_TIMEOUT_MS,
        ),
        tool_timeout_ms: milliseconds(matches, "tool_timeout_sec", DEFAULT_MCP_TOOL_TIMEOUT_MS),
        auth,
        prompt: None,
        sampling_enabled: true,
    }
}

/// A declared timeout in the milliseconds an entry stores.
///
/// A value the conversion cannot carry saturates to zero or beyond the range,
/// and the configuration decoder refuses both: the bound belongs there, where
/// every reader of an entry meets the same one.
fn milliseconds(matches: &ArgMatches, id: &str, default_ms: u64) -> u64 {
    matches
        .get_one::<f64>(id)
        .map_or(default_ms, |seconds| (seconds * 1_000.0) as u64)
}

/// The flags of `flags` that were given, in the order they are declared.
fn named(flags: &[(&str, bool)]) -> String {
    flags
        .iter()
        .filter(|(_, given)| *given)
        .map(|(flag, _)| *flag)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One optional string option, absent when it was not given and when it was
/// given empty: every check the reference makes reads an empty value as unset.
fn text<'a>(matches: &'a ArgMatches, id: &str) -> Option<&'a str> {
    matches
        .get_one::<String>(id)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn texts<'a>(matches: &'a ArgMatches, id: &str) -> Vec<&'a str> {
    matches
        .get_many::<String>(id)
        .map(|values| values.map(String::as_str).collect())
        .unwrap_or_default()
}

/// A configuration failure as the one line an error grammar prints, without
/// the prefix the store's own display adds for a caller that keeps the type.
fn mcp_failure(error: ConfigError) -> String {
    match error {
        ConfigError::InvalidMcp(message) => message,
        other => other.to_string(),
    }
}

/// Deletes the credentials first and the configuration entry second.
///
/// Reference `remove_mcp_server_and_credentials` and the reason it states: a
/// credential deletion can fail on a locked keyring, so doing it first leaves
/// the configuration untouched and needs no restore, and the write that races
/// other writers happens last.
fn remove(name: &str, environment: &McpEnvironment) -> Result<String, String> {
    let store = environment.store();
    if let Some(resource) = store.persisted_oauth_mcp_server(name, &environment.working_directory) {
        delete_mcp_oauth_credential(environment.keyring.as_ref(), &resource).map_err(
            |failure| format!("could not delete the OAuth credentials for `{name}`: {failure}"),
        )?;
    }
    let removal = store
        .persist_mcp_remove(name)
        .map_err(|error| error.to_string())?;
    Ok(if removal.removed {
        format!("Removed MCP server `{}`.", removal.name)
    } else {
        format!(
            "MCP server `{}` is not configured in the user config.",
            removal.name
        )
    })
}

// --------------------------------------------------------------------------
// The error grammar
// --------------------------------------------------------------------------

/// Which parser reports a failure, which is what decides the prog it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Root,
    Sub,
}

/// Reads one clap failure as the argparse message the reference prints.
fn translate(error: &clap::Error) -> (Scope, String) {
    match error.kind() {
        ErrorKind::MissingRequiredArgument => (
            Scope::Sub,
            format!(
                "the following arguments are required: {}",
                context(error, ContextKind::InvalidArg).join(", ")
            ),
        ),
        ErrorKind::InvalidValue => {
            let flag = flag(error);
            let value = context(error, ContextKind::InvalidValue).join(", ");
            if value.is_empty() {
                return (
                    Scope::Sub,
                    format!("argument {flag}: expected one argument"),
                );
            }
            (
                Scope::Sub,
                format!(
                    "argument {flag}: invalid choice: {} (choose from {})",
                    crate::argv::reading::python_repr(&value),
                    context(error, ContextKind::ValidValue).join(", ")
                ),
            )
        }
        // Only the two timeout options parse a value, so a value this port
        // accepted the shape of but could not read is always a float.
        ErrorKind::ValueValidation => (
            Scope::Sub,
            format!(
                "argument {}: invalid float value: {}",
                flag(error),
                crate::argv::reading::python_repr(
                    &context(error, ContextKind::InvalidValue).join(", ")
                )
            ),
        ),
        // argparse places what no parser could consume on the root parser,
        // which is why an extra positional names `vibe mcp` and not the
        // sub-command that was actually running.
        ErrorKind::UnknownArgument | ErrorKind::TooManyValues => (
            Scope::Root,
            unrecognized(&context(error, ContextKind::InvalidArg).join(" ")),
        ),
        _ => (
            Scope::Sub,
            error
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .to_owned(),
        ),
    }
}

fn unrecognized(extras: &str) -> String {
    format!("unrecognized arguments: {extras}")
}

/// The flag a failure names, without the value name clap appends to it.
fn flag(error: &clap::Error) -> String {
    context(error, ContextKind::InvalidArg)
        .first()
        .and_then(|argument| argument.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default()
}

/// One context entry, as the plain strings argparse would print.
fn context(error: &clap::Error, kind: ContextKind) -> Vec<String> {
    let strip = |value: &str| value.trim_matches(|c| c == '<' || c == '>').to_owned();
    match error.get(kind) {
        Some(ContextValue::String(value)) => vec![strip(value)],
        Some(ContextValue::Strings(values)) => values.iter().map(|value| strip(value)).collect(),
        Some(ContextValue::Number(value)) => vec![value.to_string()],
        _ => Vec::new(),
    }
}

/// argparse's `parser.error`: the usage line, one message line, exit 2.
fn fail(command: &Command, prog: &str, message: &str, stderr: &mut dyn Write) -> u8 {
    let _ = writeln!(stderr, "{}", usage_line(command));
    let _ = writeln!(stderr, "{prog}: error: {message}");
    USAGE_EXIT
}

fn write_line(stdout: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(stdout, "{message}");
    0
}

// --------------------------------------------------------------------------
// The renders
// --------------------------------------------------------------------------

fn subcommand_names(command: &Command) -> Vec<String> {
    command
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect()
}

fn prog_of(command: &Command) -> String {
    command
        .get_bin_name()
        .unwrap_or_else(|| command.get_name())
        .to_owned()
}

/// The value a flag or a positional takes, as argparse renders it.
fn value_display(argument: &Arg) -> Option<String> {
    if matches!(
        argument.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count | ArgAction::Help
    ) {
        return None;
    }
    argument
        .get_value_names()
        .and_then(<[clap::builder::Str]>::first)
        .map(|name| name.as_str().to_owned())
}

/// One argument as argparse spells it in a usage line: its first spelling and
/// the value it takes.
fn usage_form(argument: &Arg) -> String {
    let head = match (argument.get_short(), argument.get_long()) {
        (Some(short), _) => format!("-{short}"),
        (None, Some(long)) => format!("--{long}"),
        (None, None) => argument.get_id().to_string(),
    };
    match value_display(argument) {
        Some(value) => format!("{head} {value}"),
        None => head,
    }
}

/// Every spelling of one argument, joined the way a help block lists them.
fn help_form(argument: &Arg) -> String {
    if argument.is_positional() {
        return value_display(argument).unwrap_or_else(|| argument.get_id().to_string());
    }
    let value = value_display(argument);
    let mut spellings = Vec::new();
    if let Some(short) = argument.get_short() {
        spellings.push(format!("-{short}"));
    }
    if let Some(long) = argument.get_long() {
        spellings.push(format!("--{long}"));
    }
    for alias in argument.get_visible_aliases().unwrap_or_default() {
        spellings.push(format!("--{alias}"));
    }
    spellings
        .iter()
        .map(|spelling| match &value {
            Some(value) => format!("{spelling} {value}"),
            None => spelling.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The options of `command`, with the help flag first as argparse orders it.
fn options_of(command: &Command) -> Vec<&Arg> {
    let mut options: Vec<&Arg> = command
        .get_arguments()
        .filter(|argument| !argument.is_positional())
        .collect();
    options.sort_by_key(|argument| usize::from(argument.get_id() != "help"));
    options
}

/// argparse's usage line, on one line.
///
/// The reference wraps it at the terminal width; this port does not, because
/// nothing reads the wrapped shape and the wrapping rule is argparse's own
/// rather than a contract the two surfaces share.
fn usage_line(command: &Command) -> String {
    let mut parts = vec![format!("usage: {}", prog_of(command))];
    for argument in options_of(command) {
        parts.push(format!("[{}]", usage_form(argument)));
    }
    let names = subcommand_names(command);
    if !names.is_empty() {
        parts.push(format!("{{{}}} ...", names.join(",")));
    }
    for positional in command.get_positionals() {
        let form = help_form(positional);
        parts.push(if positional.is_required_set() {
            form
        } else {
            format!("[{form}]")
        });
    }
    parts.join(" ")
}

/// argparse's help column: two past the widest invocation, capped at 24.
fn help_position(entries: &[(usize, String, Option<String>)]) -> usize {
    let widest = entries
        .iter()
        .map(|(indent, invocation, _)| indent + invocation.chars().count())
        .max()
        .unwrap_or(0);
    (widest + 2).min(MAX_HELP_POSITION)
}

fn render_entry(
    out: &mut dyn Write,
    position: usize,
    indent: usize,
    invocation: &str,
    description: Option<&str>,
) {
    let padding = " ".repeat(indent);
    let Some(description) = description else {
        let _ = writeln!(out, "{padding}{invocation}");
        return;
    };
    if indent + invocation.chars().count() <= position.saturating_sub(2) {
        let width = position - indent - 2;
        let _ = writeln!(out, "{padding}{invocation:<width$}  {description}");
    } else {
        let _ = writeln!(out, "{padding}{invocation}");
        let _ = writeln!(out, "{}{description}", " ".repeat(position));
    }
}

/// argparse's help render: the usage line, the description, the positional
/// block and the options block.
fn print_help(command: &Command, stdout: &mut dyn Write) -> u8 {
    let _ = writeln!(stdout, "{}", usage_line(command));
    if let Some(description) = command.get_long_about() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "{description}");
    }
    let mut positionals: Vec<(usize, String, Option<String>)> = Vec::new();
    let names = subcommand_names(command);
    if !names.is_empty() {
        positionals.push((2, format!("{{{}}}", names.join(",")), None));
        for sub in command.get_subcommands() {
            positionals.push((
                4,
                sub.get_name().to_owned(),
                sub.get_about().map(ToString::to_string),
            ));
        }
    }
    for positional in command.get_positionals() {
        positionals.push((
            2,
            help_form(positional),
            positional.get_help().map(ToString::to_string),
        ));
    }
    let options: Vec<(usize, String, Option<String>)> = options_of(command)
        .into_iter()
        .map(|argument| {
            let description = if argument.get_id() == "help" {
                Some("show this help message and exit".to_owned())
            } else {
                argument.get_help().map(ToString::to_string)
            };
            (2, help_form(argument), description)
        })
        .collect();
    let mut every = positionals.clone();
    every.extend(options.iter().cloned());
    let position = help_position(&every);
    if !positionals.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "positional arguments:");
        for (indent, invocation, description) in &positionals {
            render_entry(
                stdout,
                position,
                *indent,
                invocation,
                description.as_deref(),
            );
        }
    }
    if !options.is_empty() {
        let _ = writeln!(stdout);
        let _ = writeln!(stdout, "options:");
        for (indent, invocation, description) in &options {
            render_entry(
                stdout,
                position,
                *indent,
                invocation,
                description.as_deref(),
            );
        }
    }
    0
}

#[cfg(test)]
mod mcp_command_tests;
