use std::io::{Read as _, Write as _};
use std::net::TcpListener;

use super::*;
use crate::workspace::WorkspacePaths;

/// A console that answers one `/whoami` with `body` and hands back the
/// request line it read.
fn one_shot_console(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let address = listener.local_addr().expect("the bound address");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("the request reads");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("the answer writes");
        String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    (format!("http://{address}"), handle)
}

/// Reference `AccountController.read`: a successful read heals the
/// configuration with the tenant hosts the console advertised, after the view
/// was built from the configuration as it stood.
#[test]
fn a_read_account_heals_the_tenant_hosts_into_the_configuration() {
    let (console, served) = one_shot_console(
        r#"{"plan_type":"api","plan_name":"Scale","api_base":"https://api.tenant.example/","vibe_base":"https://chat.tenant.example"}"#,
    );
    let temporary = tempfile::tempdir().expect("temporary");
    let vibe_home = temporary.path().join("vibe-home");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    std::fs::write(
        vibe_home.join("config.toml"),
        format!(
            "console_base_url = \"{console}\"\n\n[[providers]]\nname = \"mistral\"\n\
             api_base = \"https://api.mistral.ai/v1\"\n\
             api_key_env_var = \"VIBE_ACCOUNT_RECONCILE_TEST_KEY\"\nbackend = \"mistral\"\n"
        ),
    )
    .expect("config");
    std::fs::write(
        vibe_core::config::global_env_file(&vibe_home),
        "VIBE_ACCOUNT_RECONCILE_TEST_KEY=oracle-key\n",
    )
    .expect("dotenv");
    let working_directory = temporary.path().join("workspace");
    std::fs::create_dir_all(&working_directory).expect("workspace");
    let workspace = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory,
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("workspace service");

    let (account, lookup) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(workspace.read_account_lookup());

    assert_eq!(
        served.join().expect("the console thread"),
        "GET /api/vibe/whoami HTTP/1.1"
    );
    assert_eq!(account["status"], json!("ready"));
    assert_eq!(
        account["planOffer"]["url"],
        json!("https://chat.mistral.ai/code/extensions?focus=key"),
        "the view names the chat base it was read with"
    );
    assert!(matches!(lookup, AccountLookup::Plan { .. }));
    let written = std::fs::read_to_string(vibe_home.join("config.toml")).expect("config");
    let document: toml::Table = written.parse().expect("the healed file parses");
    assert_eq!(
        document["vibe_base_url"].as_str(),
        Some("https://chat.tenant.example")
    );
    let provider = document["providers"].as_array().expect("providers")[0]
        .as_table()
        .expect("the provider entry")
        .clone();
    assert_eq!(
        provider["api_base"].as_str(),
        Some("https://api.tenant.example/v1")
    );
}
