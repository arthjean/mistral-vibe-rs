//! `/mcp` argument parsing: the subcommand split and the `/mcp add` grammar.
//!
//! Reference `vibe/cli/textual_ui/mcp_commands.py`. The usage and help lines are
//! the operator-facing contract of the command and are reproduced as observed.

/// The subcommands `/mcp` routes before it opens the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Subcommand {
    Add,
    Login,
    Logout,
    Status,
}

pub(super) const ADD_USAGE: &str = "Usage: /mcp add <url> [--name <alias>] [--scope <scope> ...] \
                                    [--transport <http|streamable-http>] [--no-login] \
                                    [--allow-insecure-http]";

pub(super) fn add_help() -> String {
    format!(
        "{ADD_USAGE}\n\nOAuth-only shortcut for hosted MCP servers.\nDefaults to streamable-http; \
         pass --transport http for servers documented with\nHTTP transport.\nPass \
         --allow-insecure-http to allow a plaintext http:// URL on a non-localhost\nhost, such as \
         a server on the LAN.\nFor API-key/static auth, edit config.toml."
    )
}

/// Reference `parse_mcp_subcommand`: the first whitespace-separated word names
/// the subcommand, and everything after it, trimmed, is its argument string.
pub(super) fn parse_subcommand(arguments: &str) -> Option<(Subcommand, &str)> {
    let trimmed = arguments.trim();
    let split = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let subcommand = match &trimmed[..split] {
        "add" => Subcommand::Add,
        "login" => Subcommand::Login,
        "logout" => Subcommand::Logout,
        "status" => Subcommand::Status,
        _ => return None,
    };
    Some((subcommand, trimmed[split..].trim()))
}

/// Reference `is_mcp_add_help_request`.
pub(super) fn is_add_help_request(arguments: &str) -> bool {
    matches!(arguments.trim(), "--help" | "-h")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) struct AddArguments {
    pub url: String,
    pub name: Option<String>,
    pub scopes: Vec<String>,
    pub transport: &'static str,
    pub login: bool,
    pub allow_insecure_http: bool,
}

/// Reference `parse_mcp_add_args`.
pub(super) fn parse_add(arguments: &str) -> Result<AddArguments, String> {
    let tokens =
        split_posix(arguments).map_err(|error| format!("Invalid /mcp add arguments: {error}"))?;
    let mut url = None;
    let mut name = None;
    let mut scopes = Vec::new();
    let mut transport = "streamable-http";
    let mut transport_seen = false;
    let mut login = true;
    let mut allow_insecure_http = false;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        match token.as_str() {
            "--no-login" => {
                login = false;
                index += 1;
            }
            "--allow-insecure-http" => {
                allow_insecure_http = true;
                index += 1;
            }
            "--name" => {
                if name.is_some() {
                    return Err("Usage: /mcp add accepts --name only once.".to_owned());
                }
                name = Some(option_value(&tokens, index, "--name", "<alias>")?);
                index += 2;
            }
            "--transport" => {
                if transport_seen {
                    return Err("Usage: /mcp add accepts --transport only once.".to_owned());
                }
                transport =
                    match option_value(&tokens, index, "--transport", "<http|streamable-http>")?
                        .as_str()
                    {
                        "http" => "http",
                        "streamable-http" => "streamable-http",
                        _ => {
                            return Err(
                                "MCP server transport must be one of: http, streamable-http."
                                    .to_owned(),
                            );
                        }
                    };
                transport_seen = true;
                index += 2;
            }
            "--scope" => {
                scopes.push(option_value(&tokens, index, "--scope", "<scope>")?);
                index += 2;
            }
            option if option.starts_with("--") => {
                return Err(format!("Unknown /mcp add option: {option}"));
            }
            _ => {
                if url.is_some() {
                    return Err(ADD_USAGE.to_owned());
                }
                url = Some(token.clone());
                index += 1;
            }
        }
    }
    let url = url.ok_or_else(|| ADD_USAGE.to_owned())?;
    Ok(AddArguments {
        url,
        name,
        scopes,
        transport,
        login,
        allow_insecure_http,
    })
}

fn option_value(
    tokens: &[String],
    index: usize,
    option: &str,
    placeholder: &str,
) -> Result<String, String> {
    match tokens.get(index + 1) {
        Some(value) if !value.starts_with("--") => Ok(value.clone()),
        _ => Err(format!("Usage: /mcp add {option} {placeholder}")),
    }
}

/// POSIX word splitting with Python `shlex.split`'s rules and error messages:
/// single quotes are literal, a backslash inside double quotes escapes only a
/// double quote or a backslash, and an empty quoted word is a word.
pub(super) fn split_posix(input: &str) -> Result<Vec<String>, &'static str> {
    const UNTERMINATED: &str = "No closing quotation";
    const DANGLING_ESCAPE: &str = "No escaped character";
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' | '\r' | '\n' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            '\\' => {
                current.push(characters.next().ok_or(DANGLING_ESCAPE)?);
                in_token = true;
            }
            '\'' => {
                in_token = true;
                loop {
                    match characters.next().ok_or(UNTERMINATED)? {
                        '\'' => break,
                        literal => current.push(literal),
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match characters.next().ok_or(UNTERMINATED)? {
                        '"' => break,
                        '\\' => match characters.next().ok_or(DANGLING_ESCAPE)? {
                            escaped @ ('"' | '\\') => current.push(escaped),
                            other => {
                                current.push('\\');
                                current.push(other);
                            }
                        },
                        literal => current.push(literal),
                    }
                }
            }
            other => {
                current.push(other);
                in_token = true;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}
