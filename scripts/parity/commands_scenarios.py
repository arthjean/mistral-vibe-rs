"""The contexts, probes and scenarios ``commands.py`` drives through the pinned
reference's command registry and handlers.

Every context, submitted line, fixture and log-level chain here is authored for
the corpus; none is read from the reference. ``commands.py`` records what the
reference's registry and ``VibeApp`` handlers answer for each.
"""

from __future__ import annotations

from typing import Any


#: The keys excluded by the one context that exercises ``excluded_commands``.
#: Three keys rather than one, so an exclusion is observed against a command
#: with a single alias, a command carrying two, and a command whose key is the
#: first in sort order.
EXCLUDED_KEYS = ("help", "mcp", "theme")

#: Every gate the registry reads, each opened alone and all opened together,
#: plus one context carrying a non-empty excluded set. ``CommandContext``
#: (``vibe/cli/commands.py:10-13``) carries ``registry_skills_enabled`` and
#: ``experimental_harness``, and ``/paste-image`` reads the host platform; no
#: predicate reads two of them, so one context per gate reaches every branch.
CONTEXTS: tuple[dict[str, Any], ...] = (
    {
        "id": "baseline",
        "registrySkillsEnabled": False,
        "experimentalHarness": False,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "clipboard",
        "registrySkillsEnabled": False,
        "experimentalHarness": False,
        "clipboardSupported": True,
        "excluded": [],
    },
    {
        "id": "registrySkills",
        "registrySkillsEnabled": True,
        "experimentalHarness": False,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "experimentalHarness",
        "registrySkillsEnabled": False,
        "experimentalHarness": True,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "full",
        "registrySkillsEnabled": True,
        "experimentalHarness": True,
        "clipboardSupported": True,
        "excluded": [],
    },
    {
        "id": "excluded",
        "registrySkillsEnabled": True,
        "experimentalHarness": True,
        "clipboardSupported": True,
        "excluded": list(EXCLUDED_KEYS),
    },
)

#: The context the inventory and the help document are captured under: every
#: command available, so the corpus describes the whole table rather than the
#: subset the capturing host happens to allow.
FULL_CONTEXT = "full"

#: The lines a submitted command line can be, with the context each is resolved
#: under. Chosen to reach the reference's own branches rather than to sample:
#: trimming, the whitespace split, bare aliases with and without arguments,
#: Unicode case folding, availability gating and the empty input.
PARSE_PROBES: tuple[tuple[str, str, str], ...] = (
    # (id, context, input)
    ("help-slash", FULL_CONTEXT, "/help"),
    ("help-upper", FULL_CONTEXT, "/HELP"),
    ("help-mixed-case", FULL_CONTEXT, "/HeLp"),
    ("help-leading-spaces", FULL_CONTEXT, "   /help"),
    ("help-trailing-spaces", FULL_CONTEXT, "/help   "),
    ("help-surrounding-spaces", FULL_CONTEXT, "  /help  "),
    ("help-leading-tab", FULL_CONTEXT, "\t/help"),
    ("help-trailing-newline", FULL_CONTEXT, "/help\n"),
    ("mcp-with-arguments", FULL_CONTEXT, "/mcp add server"),
    ("mcp-interior-whitespace-run", FULL_CONTEXT, "/mcp    add     server"),
    ("mcp-newline-separated-arguments", FULL_CONTEXT, "/mcp\nadd server"),
    ("mcp-tab-separated-arguments", FULL_CONTEXT, "/mcp\tadd"),
    ("connectors-alias", FULL_CONTEXT, "/connectors"),
    ("connectors-alias-with-arguments", FULL_CONTEXT, "/connectors add https://example.test"),
    ("new-alias", FULL_CONTEXT, "/new"),
    ("continue-alias", FULL_CONTEXT, "/continue"),
    ("exit-bare", FULL_CONTEXT, "exit"),
    ("quit-bare", FULL_CONTEXT, "quit"),
    ("colon-q-bare", FULL_CONTEXT, ":q"),
    ("colon-quit-bare", FULL_CONTEXT, ":quit"),
    ("exit-bare-with-arguments", FULL_CONTEXT, "exit now"),
    ("quit-bare-with-arguments", FULL_CONTEXT, "quit please stop"),
    ("colon-q-bare-with-arguments", FULL_CONTEXT, ":q now"),
    ("exit-bare-uppercase", FULL_CONTEXT, "EXIT"),
    ("exit-bare-surrounding-spaces", FULL_CONTEXT, "  exit  "),
    ("exit-slash", FULL_CONTEXT, "/exit"),
    ("exit-slash-with-arguments", FULL_CONTEXT, "/exit now"),
    ("empty", FULL_CONTEXT, ""),
    ("spaces-only", FULL_CONTEXT, "   "),
    ("tabs-only", FULL_CONTEXT, "\t\t"),
    ("newline-only", FULL_CONTEXT, "\n"),
    ("unknown-slash", FULL_CONTEXT, "/nope"),
    ("unknown-bare", FULL_CONTEXT, "nope"),
    ("unknown-slash-with-arguments", FULL_CONTEXT, "/nope arg"),
    ("double-slash", FULL_CONTEXT, "//help"),
    ("slash-only", FULL_CONTEXT, "/"),
    ("kelvin-sign-thinking", FULL_CONTEXT, "/THINKING"),
    ("kelvin-sign-in-arguments", FULL_CONTEXT, "/compact Kelvin"),
    ("dotted-capital-i-thinking", FULL_CONTEXT, "/THİNKING"),
    ("fullwidth-help", FULL_CONTEXT, "/ＨＥＬＰ"),
    ("compact-with-instructions", FULL_CONTEXT, "/compact focus on the parser"),
    ("loop-with-arguments", FULL_CONTEXT, "/loop 5m check the build"),
    ("rename-with-quoted-argument", FULL_CONTEXT, '/rename "My Session"'),
    ("retry-with-long-argument", FULL_CONTEXT, "/retry " + "continue the response " * 12),
    ("paste-image-full", FULL_CONTEXT, "/paste-image"),
    ("paste-image-baseline", "baseline", "/paste-image"),
    ("paste-image-experimental-harness", "experimentalHarness", "/paste-image"),
    ("paste-image-clipboard", "clipboard", "/paste-image"),
    ("teleport-full", FULL_CONTEXT, "/teleport"),
    ("teleport-baseline", "baseline", "/teleport"),
    ("teleport-experimental-harness", "experimentalHarness", "/teleport"),
    ("remote-project-baseline", "baseline", "/remote-project"),
    ("status-baseline", "baseline", "/status"),
    ("skills-baseline", "baseline", "/skills"),
    ("skills-registry-skills", "registrySkills", "/skills"),
    ("todo-baseline", "baseline", "/todo"),
    ("todo-experimental-harness", "experimentalHarness", "/todo"),
    ("help-excluded", "excluded", "/help"),
    ("connectors-excluded", "excluded", "/connectors"),
    ("theme-excluded", "excluded", "/theme"),
    ("status-excluded", "excluded", "/status"),
)


#: The instant the loop list measures "next in" from, so the capture is stable.
LOOP_CLOCK = 1_800_000_000.0


#: ``(id, line, context, fixture)``: every command, and every branch of each
#: handler an operator can reach, over fixtures this script authors. The
#: context defaults to every gate open, so a gated command is reachable.
HANDLER_SCENARIOS: tuple[tuple[str, str, dict[str, Any], dict[str, Any]], ...] = (
    ("help", "/help", {}, {}),
    ("config", "/config", {}, {}),
    ("config-arguments", "/config set theme dark", {}, {}),
    ("model", "/model", {}, {}),
    ("model-arguments", "/model devstral-small", {}, {}),
    ("skills", "/skills", {}, {"installedSkills": [], "skillCatalog": {"skills": [], "updates": [], "project_available": False, "loaded": False, "authenticated": False}}),
    ("thinking", "/thinking", {}, {}),
    ("thinking-arguments", "/thinking high", {}, {}),
    ("reload", "/reload", {}, {}),
    ("reload-one-image", "/reload", {}, {"reload": {"strippedImages": 1}, "activeModelDisplayName": "Devstral Small"}),
    ("reload-images", "/reload", {}, {"reload": {"strippedImages": 3}, "activeModelDisplayName": "Devstral Small"}),
    ("reload-failure", "/reload", {}, {"reload": {"raise": "the configuration file is unreadable"}}),
    ("clear", "/clear", {}, {}),
    ("clear-new-alias", "/new", {}, {}),
    ("clear-not-persisted", "/clear", {}, {"sessionLog": {"persisted": False}}),
    ("clear-logging-disabled", "/clear", {}, {"sessionLog": {"enabled": False}}),
    ("clear-seed", "/clear   write the missing tests  ", {}, {}),
    ("clear-failure", "/clear seed", {}, {"clearHistory": {"raise": "the session store is locked"}}),
    ("copy-nothing", "/copy", {}, {}),
    ("copy", "/copy", {}, {"lastAssistantMessage": "The answer is 42."}),
    ("copy-unverified", "/copy", {}, {"lastAssistantMessage": "The answer is 42.", "clipboardVerified": False}),
    ("paste-image", "/paste-image", {}, {"clipboardSupported": True}),
    ("log", "/log", {}, {}),
    ("log-disabled", "/log", {}, {"sessionLog": {"enabled": False}}),
    ("log-not-persisted", "/log", {}, {"sessionLog": {"persisted": False}}),
    ("log-level", "/log-level", {}, {}),
    ("log-level-arguments", "/log-level debug", {}, {}),
    ("debug", "/debug", {}, {}),
    ("compact", "/compact", {}, {"history": ["entry"]}),
    ("compact-instructions", "/compact   keep the API notes ", {}, {"history": ["entry"]}),
    ("compact-empty", "/compact", {}, {"history": []}),
    ("compact-busy", "/compact", {}, {"history": ["entry"], "turnActive": True}),
    ("compact-failure", "/compact", {}, {"history": ["entry"], "compact": {"raise": "the summary could not be produced"}}),
    ("exit", "/exit", {}, {}),
    ("exit-bare", "quit", {}, {}),
    ("status", "/status", {}, {}),
    ("status-cached", "/status", {}, {"stats": {"steps": 7, "sessionPromptTokens": 12345, "sessionCachedTokens": 1000, "sessionCompletionTokens": 2345, "sessionTotalLlmTokens": 14690, "lastTurnTotalTokens": 3210, "lastTurnCachedTokens": 512, "sessionCost": 0.12345}}),
    ("status-uncached", "/status", {}, {"stats": {"steps": 1234567, "sessionPromptTokens": 9876543, "sessionCompletionTokens": 1, "sessionTotalLlmTokens": 9876544, "lastTurnTotalTokens": 42, "sessionCost": 1234.5}}),
    ("status-unified", "/status", {}, {"experimentalHarness": True, "providerAuth": {"model_display_name": "Devstral 2", "provider_name": "mistral", "api_base": "https://api.mistral.ai/v1"}}),
    ("status-unified-cached", "/status", {}, {"experimentalHarness": True, "stats": {"steps": 3, "sessionPromptTokens": 2048, "sessionCachedTokens": 512, "sessionCompletionTokens": 64, "sessionTotalLlmTokens": 2112, "lastTurnTotalTokens": 700, "lastTurnCachedTokens": 128, "sessionCost": 0.5}, "providerAuth": {"model_display_name": "local-model", "provider_name": "llamacpp", "api_base": "http://127.0.0.1:8080/v1"}}),
    ("status-unified-invalid-base", "/status", {}, {"experimentalHarness": True, "providerAuth": {"model_display_name": "Devstral 2", "provider_name": "mistral", "api_base": None}}),
    ("status-unified-markup", "/status", {}, {"experimentalHarness": True, "providerAuth": {"model_display_name": "[bold]*x*_y_ `c` <a> & #h !b", "provider_name": "a\\b~c~~d~ ~~e", "api_base": "https://host.example:8443/v1.2/x_y"}}),
    ("status-unified-control", "/status", {}, {"experimentalHarness": True, "providerAuth": {"model_display_name": "line\nbreak\ttab\x7fdel\x85next", "provider_name": "~\x00~", "api_base": "https://h.example/p"}}),
    ("status-unified-failure", "/status", {}, {"experimentalHarness": True, "providerAuth": {"raise": "the provider view is unavailable"}}),
    ("status-unified-response-error", "/status", {}, {"experimentalHarness": True, "providerAuth": {"error": "Provider auth is not supported"}}),
    ("whoami-no-identity", "/whoami", {}, {}),
    ("whoami", "/whoami", {}, {"identity": {"name": "Ada Lovelace", "email": "ada@example.test", "workspace": {"id": "w", "name": "Analytical"}, "organization": {"id": "o", "name": "Engines"}}, "account": {"plan": {"title": "Pro"}}}),
    ("whoami-name-is-email", "/whoami", {}, {"identity": {"name": "ada@example.test", "email": "ada@example.test", "workspace": None, "organization": None}, "account": {"plan": None}}),
    ("whoami-sparse", "/whoami", {}, {"identity": {"name": None, "email": None, "workspace": None, "organization": None}}),
    ("whoami-identity-failure", "/whoami", {}, {"identity": {"raise": "unreachable"}, "account": {"plan": {"title": "Pro"}}}),
    ("whoami-account-failure", "/whoami", {}, {"identity": {"name": "Ada", "email": "ada@example.test", "workspace": None, "organization": None}, "account": {"raise": "unreachable"}}),
    ("teleport", "/teleport", {}, {}),
    ("teleport-arguments", "/teleport ship it", {}, {}),
    ("remote-project", "/remote-project", {}, {"openProjects": ["view", "picker"]}),
    ("remote-project-failure", "/remote-project", {}, {"openProjects": {"error": "Vibe Code is unavailable"}}),
    ("proxy-setup", "/proxy-setup", {}, {}),
    ("proxy-setup-arguments", "/proxy-setup HTTPS_PROXY http://proxy.test", {}, {}),
    ("resume", "/resume", {}, {"savedSessions": 2}),
    ("resume-continue-alias", "/continue", {}, {"savedSessions": 1}),
    ("resume-arguments", "/resume session-1", {}, {"savedSessions": 1}),
    ("resume-none", "/resume", {}, {}),
    ("resume-logging-disabled", "/resume", {}, {"savedSessions": 2, "sessionLog": {"enabled": False}}),
    ("rename-empty", "/rename   ", {}, {}),
    ("rename", "/rename  Parser rewrite ", {}, {}),
    ("rename-failure", "/rename Parser rewrite", {}, {"rename": {"raise": "the title is too long"}}),
    ("mcp-empty", "/mcp", {}, {}),
    ("mcp-connectors-alias", "/connectors", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp", "/mcp", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-named", "/mcp search", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-unknown", "/mcp nothing", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-connector-error", "/mcp", {}, {"mcp": {"connectorError": "the connector catalog timed out"}}),
    ("mcp-connector-error-with-sources", "/mcp", {}, {"mcp": {"sources": ["docs"], "connectorError": "the connector catalog timed out"}}),
    ("mcp-status-empty", "/mcp status", {}, {}),
    ("mcp-status", "/mcp status", {}, {"mcp": {"statuses": {"search": "authenticated", "docs": "needs_login"}}}),
    ("mcp-status-arguments", "/mcp status extra", {}, {}),
    ("mcp-login-usage", "/mcp login", {}, {}),
    ("mcp-login", "/mcp login docs", {}, {"mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-login-connector", "/mcp login search", {}, {"mcp": {"sources": ["docs"], "connectors": ["search"]}}),
    ("mcp-login-shared-alias", "/mcp login docs", {}, {"mcp": {"sources": ["docs"], "connectors": ["docs"]}, "mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-connectors-listed", "/mcp search", {}, {"mcp": {"sources": ["docs"], "connectors": ["search"]}}),
    ("mcp-login-failure", "/mcp login docs", {}, {"mcpLogin": {"error": "Unknown MCP server: docs"}}),
    ("mcp-logout-usage", "/mcp logout   ", {}, {}),
    ("mcp-logout", "/mcp logout docs", {}, {}),
    ("mcp-logout-failure", "/mcp logout docs", {}, {"mcpLogout": {"error": "Unknown MCP server: docs"}}),
    ("mcp-add-help", "/mcp add --help", {}, {}),
    ("mcp-add-short-help", "/mcp add -h", {}, {}),
    ("mcp-add-usage", "/mcp add", {}, {}),
    ("mcp-add-two-urls", "/mcp add https://a.test https://b.test", {}, {}),
    ("mcp-add-unknown-option", "/mcp add https://a.test --verbose", {}, {}),
    ("mcp-add-name-twice", "/mcp add https://a.test --name a --name b", {}, {}),
    ("mcp-add-name-missing", "/mcp add https://a.test --name", {}, {}),
    ("mcp-add-transport-twice", "/mcp add https://a.test --transport http --transport http", {}, {}),
    ("mcp-add-transport-invalid", "/mcp add https://a.test --transport stdio", {}, {}),
    ("mcp-add-scope-missing", "/mcp add https://a.test --scope --no-login", {}, {}),
    ("mcp-add-quoting", "/mcp add 'https://a.test", {}, {}),
    ("mcp-add", "/mcp add https://mcp.example.test --no-login", {}, {"mcpAdd": {"name": "mcp-example"}}),
    ("mcp-add-options", "/mcp add https://mcp.example.test --name docs --scope read --scope write --transport http --allow-insecure-http --no-login", {}, {"mcpAdd": {"name": "docs"}}),
    ("mcp-add-existing", "/mcp add https://mcp.example.test --no-login", {}, {"mcpAdd": {"name": "docs", "created": False}}),
    ("mcp-add-login", "/mcp add https://mcp.example.test --name docs", {}, {"mcpAdd": {"name": "docs"}, "mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-add-failure", "/mcp add https://mcp.example.test", {}, {"mcpAdd": {"error": "An MCP server named docs already exists"}}),
    ("plugins", "/plugins", {}, {}),
    ("reload-plugins", "/reload-plugins", {}, {}),
    ("todo", "/todo", {}, {}),
    ("voice", "/voice", {}, {}),
    ("leanstall", "/leanstall", {}, {"agents": ["default", "plan"]}),
    ("leanstall-present", "/leanstall", {}, {"agents": ["default", "lean"]}),
    ("unleanstall", "/unleanstall", {}, {"agents": ["default", "lean"]}),
    ("unleanstall-absent", "/unleanstall", {}, {"agents": ["default"]}),
    ("rewind", "/rewind", {}, {}),
    ("branch", "/branch", {}, {}),
    ("branch-arguments", "/branch ignored words", {}, {}),
    ("branch-failure", "/branch", {}, {"fork": {"raise": "the session has no history yet"}}),
    ("retry-not-offered", "/retry", {}, {}),
    ("retry", "/retry", {}, {"retryOffered": True}),
    ("retry-instructions", "/retry  keep it short ", {}, {"retryOffered": True}),
    ("retry-busy", "/retry", {}, {"retryOffered": True, "turnActive": True}),
    ("loop-list-empty", "/loop", {}, {}),
    ("loop-list", "/loop list", {}, {"loops": {"list": [{"id": "loop-1", "prompt": "check | the build\nnow", "interval_seconds": 3700, "next_fire_at": LOOP_CLOCK + 125}, {"id": "loop-2", "prompt": "ping", "interval_seconds": 86400, "next_fire_at": LOOP_CLOCK - 5}]}}),
    ("loop-ls", "/loop LS", {}, {}),
    ("loop-create", "/loop 5m   check the build  ", {}, {"loops": {"created": {"id": "loop-7", "interval_seconds": 300}}}),
    ("loop-create-interval-only", "/loop 90s", {}, {"loops": {"created": {"id": "loop-8", "interval_seconds": 90}}}),
    ("loop-cancel", "/loop cancel loop-1", {}, {"loops": {"deletedPrompt": "check the build"}}),
    ("loop-cancel-verb", "/loop RM loop-1", {}, {"loops": {"deletedPrompt": "check the build"}}),
    ("loop-cancel-all", "/loop stop all", {}, {"loops": {"cleared": 3}}),
    ("loop-cancel-missing", "/loop delete", {}, {}),
    ("loop-error", "/loop 5x check", {}, {"loops": {"error": "Invalid interval: 5x"}}),
    ("data-retention", "/data-retention", {}, {}),
    ("theme", "/theme", {}, {}),
)


#: The composer states a submitted line can meet: nothing running, a model
#: turn, a shell command, and each of those with the queue paused.
DISPATCH_STATES = ("idle", "busy", "shell", "paused", "pausedBusy", "pausedShell")

#: One line of every kind ``classify`` tells apart, plus the side-channel and
#: exiting commands, a slash line that names nothing, and the command the
#: registry forwards to the model under the Unified Harness every dispatch
#: registry opens (``/loop``, ``vibe/cli/commands.py:232-242``).
DISPATCH_INPUTS = (
    ("side-channel", "/status"),
    ("side-channel-exit", "exit"),
    ("command", "/model"),
    ("prompt", "hello"),
    ("skill", "/review this change"),
    ("shell", "!ls -la"),
    ("empty-shell", "!"),
    ("teleport", "&ship it"),
    ("unknown-slash", "/nothing"),
    ("forwarded", "/loop 5m check the build"),
)


#: ``(id, chain, applied, persist failure)`` for the panel's apply handler.
LOG_LEVEL_APPLY: tuple[tuple[str, dict[str, Any], dict[str, Any], str | None], ...] = (
    ("unchanged", {}, {"session": None, "config": None, "cleared": False}, None),
    ("session", {}, {"session": "DEBUG", "config": None, "cleared": False}, None),
    ("session-cleared", {"session": "INFO"}, {"session": None, "config": None, "cleared": False}, None),
    ("config", {}, {"session": None, "config": "ERROR", "cleared": False}, None),
    ("config-kept", {"config": "INFO"}, {"session": None, "config": "INFO", "cleared": False}, None),
    ("config-cleared", {"config": "INFO"}, {"session": None, "config": None, "cleared": True}, None),
    ("both", {"session": "ERROR", "config": "INFO"}, {"session": "DEBUG", "config": "CRITICAL", "cleared": False}, None),
    ("env-wins", {"env": "INFO"}, {"session": None, "config": "ERROR", "cleared": False}, None),
    ("debug-mode", {"debugMode": True}, {"session": None, "config": None, "cleared": False}, None),
    ("persist-failure", {}, {"session": "DEBUG", "config": "ERROR", "cleared": False}, "the configuration file is read-only"),
)


#: ``(id, chain, actions)`` for the panel itself. An action is ``highlight``
#: with a level, ``badge`` with ``session`` or ``config``, or ``toggle``.
LOG_LEVEL_PICKER: tuple[tuple[str, dict[str, Any], list[list[str]]], ...] = (
    ("open-default", {}, []),
    ("open-session", {"session": "DEBUG", "config": "ERROR"}, []),
    ("open-env", {"env": "INFO"}, []),
    ("open-config", {"config": "ERROR"}, []),
    ("set-session", {}, [["highlight", "INFO"], ["toggle"]]),
    ("unset-session", {"session": "INFO"}, [["toggle"]]),
    ("set-config", {}, [["highlight", "CRITICAL"], ["badge", "config"], ["toggle"]]),
    ("clear-config", {"config": "ERROR"}, [["highlight", "ERROR"], ["badge", "config"], ["toggle"]]),
    ("move-config", {"config": "ERROR"}, [["highlight", "DEBUG"], ["badge", "config"], ["toggle"]]),
    ("back-to-session", {}, [["badge", "config"], ["badge", "session"], ["highlight", "ERROR"], ["toggle"]]),
    ("both", {"env": "WARNING"}, [["highlight", "DEBUG"], ["toggle"], ["badge", "config"], ["highlight", "INFO"], ["toggle"]]),
)
