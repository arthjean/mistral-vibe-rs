//! The Codex adapter against what the reference resolver produced for the
//! same trees, captured with `PluginResolver(user_roots=..., data_root_base=...)`
//! at the pinned commit. Paths are spelled relative to the plugin and data
//! roots; diagnostic prose is this port's own and is not compared.

use std::fs;

use serde_json::{Value, json};

use crate::plugins::compatibility::{PluginMcpHttpAuth, PluginMcpServer};
use crate::plugins::native::PluginResolver;

/// The fixture tree, keyed by path under the user plugin root.
const SPEC: &str = r#"{
 "full/.codex-plugin/plugin.json": "{\"name\": \"Codex Demo\", \"version\": \" 1.2 \", \"author\": {\"name\": \" Ann \"}, \"skills\": [\"./extra-skills\", \"./missing\"], \"mcpServers\": \"./servers.json\", \"interface\": {\"displayName\": \"Demo\"}, \"apps\": \"./app.json\", \"hooks\": [{\"event\": \"start\"}], \"homepage\": \"https://example.com\"}\n",
 "full/skills/alpha/SKILL.md": "---\nname: alpha\ndescription: Alpha skill for the probe\n---\nAlpha.\n",
 "full/skills/alpha/agents/openai.yml": "policy: {}\n",
 "full/extra-skills/beta/SKILL.md": "---\nname: beta\ndescription: Beta skill for the probe\n---\nBeta.\n",
 "full/extra-skills/beta/agents/openai.yaml": "policy:\n  allow_implicit_invocation: false\n",
 "full/extra-skills/gamma/SKILL.md": "---\nname: gamma\ndescription: Gamma skill for the probe\n---\nGamma.\n",
 "full/extra-skills/gamma/agents/openai.yaml": "interface:\n  display_name: Gamma\n",
 "full/.mcp.json": "{\"mcpServers\": {\"local\": {\"command\": \"./bin/server\", \"args\": [\"--port\", \"1\"], \"env\": {\"A\": \"b\"}, \"cwd\": \"./work\", \"note\": \"n\", \"startup_timeout_sec\": 5}, \"dup\": {\"command\": \"node\"}, \"badcmd\": {\"command\": \"bin/x\"}, \"reserved\": {\"command\": \"node\", \"env\": {\"PLUGIN_ROOT\": \"x\"}}}}\n",
 "full/servers.json": "{\"mcpServers\": {\"dup\": {\"type\": \"http\", \"url\": \"https://example.com/dup\"}, \"remote\": {\"type\": \"http\", \"url\": \"https://example.com/mcp?k=v\", \"headers\": {\"X-A\": \"1\"}, \"bearer_token_env_var\": \"CODEX_PROBE_TOKEN\"}, \"oauthy\": {\"type\": \"http\", \"url\": \"https://example.com/o\", \"headers\": {\"bad header\": \"1\"}, \"oauth_resource\": \"https://example.com\"}, \"emptyurl\": {\"type\": \"http\", \"url\": \"\"}, \"sse\": {\"type\": \"sse\", \"url\": \"https://example.com/s\"}, \"badheader\": {\"type\": \"http\", \"url\": \"https://example.com/h\", \"headers\": {\"bad header\": \"1\"}}, \"!!!\": {\"command\": \"node\"}}}\n",
 "full/app.json": "{}\n",
 "full/agents/readme.md": "agent notes\n",
 "full/work/.keep": "",
 "minimal/.codex-plugin/plugin.json": "{\"skills\": 5, \"mcpServers\": 3, \"interface\": \"x\", \"hooks\": \"./hooks.json\", \"apps\": 7}\n",
 "minimal/hooks.json": "{}\n",
 "minimal/.app.json": "{}\n",
 "implicit/.codex-plugin/plugin.json": "{\"name\": \"implicit\", \"description\": \"\", \"mcpServers\": {\"inline\": {\"type\": \"http\", \"url\": \"https://example.com/i\"}}}\n",
 "implicit/hooks.json": "{}\n",
 "implicit/.app.json": "{}\n",
 "implicit/skills/one/SKILL.md": "---\nname: one\ndescription: One skill for the probe\n---\nOne.\n",
 "broken/.codex-plugin/plugin.json": "{not json\n",
 "badname/.codex-plugin/plugin.json": "{\"name\": 7}\n"
}"#;

/// The reference's projection of the resolve.
const EXPECTED: &str = r#"{
 "plugins": [
  {
   "name": "Codex Demo",
   "namespace": "Codex_Demo",
   "version": "1.2",
   "description": "Capabilities provided by Codex Demo.",
   "author": "Ann",
   "manifest_digest": "244b58336048e861d3d764f9093e5366788e4cf719a271225d75cb6178dab213",
   "content_digest": "af95abbfa308a276e6f9202fb01f51da404afc15ea961a6b71bcca0d4ea0d840",
   "data_root": "<data>/Codex_Demo",
   "metadata_keys": [
    "codexManifest",
    "codexMcp"
   ]
  },
  {
   "name": "implicit",
   "namespace": "implicit",
   "version": null,
   "description": "Capabilities provided by implicit.",
   "author": null,
   "manifest_digest": "af62cede316ac3b69f093e0eb80206e69789c071df954169262be93a52ca0364",
   "content_digest": "f4293c802b1e76327074ed1d7d83ec28753a0f174bfadf513ccb7a37189aec3c",
   "data_root": "<data>/implicit",
   "metadata_keys": [
    "codexManifest",
    "codexMcp"
   ]
  },
  {
   "name": "minimal",
   "namespace": "minimal",
   "version": null,
   "description": "Capabilities provided by minimal.",
   "author": null,
   "manifest_digest": "88fdfb3914d1eaa78ac4fa1d9e1e606cc15fcacaca7997ef15fcda97e4a45b7e",
   "content_digest": "14e67db0136229869c98ab9a32463c73c8f830ac19899de947c7f6f9f30a983f",
   "data_root": "<data>/minimal",
   "metadata_keys": [
    "codexManifest"
   ]
  }
 ],
 "skills": [
  [
   "Codex_Demo:beta",
   "<plugins>/full/extra-skills/beta/SKILL.md",
   false
  ],
  [
   "Codex_Demo:gamma",
   "<plugins>/full/extra-skills/gamma/SKILL.md",
   true
  ],
  [
   "Codex_Demo:alpha",
   "<plugins>/full/skills/alpha/SKILL.md",
   true
  ],
  [
   "implicit:one",
   "<plugins>/implicit/skills/one/SKILL.md",
   true
  ]
 ],
 "mcp": [
  [
   "Codex Demo",
   "local",
   "plugin_7f486ca5253846ce_local",
   "<plugins>/full/.mcp.json",
   "stdio"
  ],
  [
   "Codex Demo",
   "oauthy",
   "plugin_3a82d92d79238122_oauthy",
   "<plugins>/full/servers.json",
   "http"
  ],
  [
   "Codex Demo",
   "remote",
   "plugin_fc45f8776bf0959d_remote",
   "<plugins>/full/servers.json",
   "http"
  ],
  [
   "implicit",
   "inline",
   "plugin_7420faf3e530bbfc_inline",
   "<plugins>/implicit/.codex-plugin/plugin.json",
   "http"
  ]
 ],
 "issues": [
  [
   "<plugins>/badname/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.manifest_invalid",
   "error",
   true,
   "manifest"
  ],
  [
   "<plugins>/broken/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.manifest_invalid",
   "error",
   true,
   "manifest"
  ],
  [
   "<plugins>/full/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.hooks_unsupported",
   "warning",
   false,
   "hook"
  ],
  [
   "<plugins>/full/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.interface_metadata_unsupported",
   "info",
   false,
   "interface"
  ],
  [
   "<plugins>/full/.mcp.json",
   "plugin.compatibility.codex.mcp_metadata_partially_supported",
   "info",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/.mcp.json",
   "plugin.compatibility.codex.mcp_server_collision",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/.mcp.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/.mcp.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/agents/readme.md",
   "plugin.compatibility.codex.agent_metadata_unsupported",
   "info",
   false,
   "agent_metadata"
  ],
  [
   "<plugins>/full/app.json",
   "plugin.compatibility.codex.openai_app_unsupported",
   "warning",
   false,
   "app"
  ],
  [
   "<plugins>/full/extra-skills/gamma/agents/openai.yaml",
   "plugin.compatibility.codex.agent_metadata_unsupported",
   "info",
   false,
   "agent_metadata"
  ],
  [
   "<plugins>/full/missing",
   "plugin.path.outside_root",
   "error",
   false,
   "skill"
  ],
  [
   "<plugins>/full/servers.json",
   "plugin.compatibility.codex.mcp_metadata_partially_supported",
   "info",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/servers.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/servers.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/servers.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/servers.json",
   "plugin.compatibility.codex.mcp_server_invalid",
   "error",
   false,
   "mcp_server"
  ],
  [
   "<plugins>/full/skills/alpha/agents/openai.yml",
   "plugin.compatibility.codex.agent_metadata_unsupported",
   "info",
   false,
   "agent_metadata"
  ],
  [
   "<plugins>/implicit/.app.json",
   "plugin.compatibility.codex.openai_app_unsupported",
   "warning",
   false,
   "app"
  ],
  [
   "<plugins>/implicit/hooks.json",
   "plugin.compatibility.codex.hooks_unsupported",
   "warning",
   false,
   "hook"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.app_invalid",
   "warning",
   false,
   "app"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.interface_metadata_invalid",
   "warning",
   false,
   "interface"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.interface_metadata_unsupported",
   "info",
   false,
   "interface"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.mcp_declaration_invalid",
   "error",
   false,
   "mcp"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.openai_app_unsupported",
   "warning",
   false,
   "app"
  ],
  [
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "plugin.compatibility.codex.skills_declaration_invalid",
   "error",
   false,
   "skill"
  ],
  [
   "<plugins>/minimal/hooks.json",
   "plugin.compatibility.codex.hooks_unsupported",
   "warning",
   false,
   "hook"
  ]
 ],
 "unsupported": [
  [
   "Codex Demo",
   "agent_metadata",
   "<plugins>/full/agents/readme.md",
   "codex_agent_metadata_unsupported"
  ],
  [
   "Codex Demo",
   "agent_metadata",
   "<plugins>/full/extra-skills/gamma/agents/openai.yaml",
   "codex_agent_metadata_unsupported"
  ],
  [
   "Codex Demo",
   "agent_metadata",
   "<plugins>/full/skills/alpha/agents/openai.yml",
   "codex_agent_metadata_unsupported"
  ],
  [
   "Codex Demo",
   "codex_hooks",
   "<plugins>/full/.codex-plugin/plugin.json",
   "codex_hook_semantics_unsupported"
  ],
  [
   "Codex Demo",
   "interface_metadata",
   "<plugins>/full/.codex-plugin/plugin.json",
   "codex_interface_metadata_runtime_private"
  ],
  [
   "Codex Demo",
   "openai_app",
   "<plugins>/full/app.json",
   "openai_apps_ui_unsupported"
  ],
  [
   "implicit",
   "codex_hooks",
   "<plugins>/implicit/hooks.json",
   "codex_hook_semantics_unsupported"
  ],
  [
   "implicit",
   "openai_app",
   "<plugins>/implicit/.app.json",
   "openai_apps_ui_unsupported"
  ],
  [
   "minimal",
   "codex_hooks",
   "<plugins>/minimal/hooks.json",
   "codex_hook_semantics_unsupported"
  ],
  [
   "minimal",
   "interface_metadata",
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "codex_interface_metadata_runtime_private"
  ],
  [
   "minimal",
   "openai_app",
   "<plugins>/minimal/.codex-plugin/plugin.json",
   "openai_apps_ui_unsupported"
  ]
 ]
}"#;

#[test]
fn codex_plugins_resolve_as_the_reference_resolves_them() {
    let temporary = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temporary.path()).unwrap();
    let plugins = base.join("plugins");
    let spec: serde_json::Map<String, Value> = serde_json::from_str(SPEC).unwrap();
    for (relative, content) in &spec {
        let path = plugins.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content.as_str().unwrap()).unwrap();
    }
    let data = base.join("plugin-data");
    let resolved = PluginResolver {
        user_roots: vec![plugins.clone()],
        data_root_base: Some(data.clone()),
        ..PluginResolver::default()
    }
    .resolve();
    let rel = |path: &std::path::Path| {
        path.to_string_lossy()
            .replace(&*plugins.to_string_lossy(), "<plugins>")
            .replace(&*data.to_string_lossy(), "<data>")
    };

    let mut issues: Vec<Value> = resolved
        .issues
        .iter()
        .map(|issue| {
            json!([
                rel(&issue.file),
                issue.code,
                issue.severity.as_str(),
                issue.fatal,
                issue.component
            ])
        })
        .collect();
    issues.sort_by_key(ToString::to_string);
    let mut unsupported: Vec<Value> = resolved
        .unsupported_components
        .iter()
        .map(|item| json!([item.plugin_name, item.kind, rel(&item.path), item.reason]))
        .collect();
    unsupported.sort_by_key(ToString::to_string);
    let actual = json!({
        "plugins": resolved.plugins.iter().map(|plugin| json!({
            "name": plugin.name,
            "namespace": plugin.namespace,
            "version": plugin.version,
            "description": plugin.description,
            "author": plugin.author,
            "manifest_digest": plugin.manifest_digest,
            "content_digest": plugin.content_digest,
            "data_root": rel(&plugin.data_root),
            "metadata_keys": plugin.private_metadata.keys().collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "skills": resolved.skills.iter().map(|(alias, skill)| json!([
            alias,
            rel(skill.path.as_deref().unwrap()),
            skill.model_invocable
        ])).collect::<Vec<_>>(),
        "mcp": resolved.mcp_servers.iter().map(|server| json!([
            server.plugin_name,
            server.source_id,
            server.private_alias,
            rel(&server.config_file),
            server.server.transport()
        ])).collect::<Vec<_>>(),
        "issues": issues,
        "unsupported": unsupported,
    });
    // The reference sorts its lists with Python's ordering on JSON arrays; the
    // comparison is by membership for the two sorted lists.
    let mut expected: Value = serde_json::from_str(EXPECTED).unwrap();
    for key in ["issues", "unsupported"] {
        if let Some(Value::Array(items)) = expected.get_mut(key) {
            items.sort_by_key(ToString::to_string);
        }
    }
    assert_eq!(actual, expected);

    let server = |source: &str| {
        resolved
            .mcp_servers
            .iter()
            .find(|server| server.source_id == source)
            .map(|server| server.server.clone())
            .unwrap()
    };
    let PluginMcpServer::Stdio {
        command,
        args,
        env,
        cwd,
        ..
    } = server("local")
    else {
        panic!("local is a stdio server");
    };
    assert_eq!(
        command
            .iter()
            .map(|part| rel(part.as_ref()))
            .collect::<Vec<_>>(),
        ["<plugins>/full/bin/server"]
    );
    assert_eq!(args, ["--port", "1"]);
    assert_eq!(rel(cwd.unwrap().as_ref()), "<plugins>/full/work");
    let env: Vec<(String, String)> = env
        .into_iter()
        .map(|(name, value)| (name, rel(value.as_ref())))
        .collect();
    assert_eq!(
        env,
        [
            ("A".to_owned(), "b".to_owned()),
            ("PLUGIN_DATA".to_owned(), "<data>/Codex_Demo".to_owned()),
            ("PLUGIN_ROOT".to_owned(), "<plugins>/full".to_owned()),
        ]
    );
    let PluginMcpServer::AuthenticatedHttp {
        url, headers, auth, ..
    } = server("remote")
    else {
        panic!("remote authenticates with a bearer token");
    };
    assert_eq!(url, "https://example.com/mcp?k=v");
    assert_eq!(headers.get("X-A").map(String::as_str), Some("1"));
    assert_eq!(
        auth,
        PluginMcpHttpAuth::BearerTokenEnv("CODEX_PROBE_TOKEN".to_owned())
    );
    let PluginMcpServer::AuthenticatedHttp { headers, auth, .. } = server("oauthy") else {
        panic!("oauthy authenticates with OAuth");
    };
    assert!(headers.is_empty());
    assert_eq!(auth, PluginMcpHttpAuth::OAuth);
    let PluginMcpServer::Http {
        transport,
        url,
        headers,
        ..
    } = server("inline")
    else {
        panic!("inline is a static HTTP server");
    };
    assert_eq!(transport, "http");
    assert_eq!(url, "https://example.com/i");
    assert!(headers.is_empty());
}
