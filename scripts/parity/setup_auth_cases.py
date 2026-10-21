"""The cases ``setup_auth.py`` drives the reference's authentication logic with.

Each family's table names the inputs a capture feeds one seam: the provenance
matrix, the scripted sign-in polls and gateway answers, the server URLs, the
tenant values and console answers, and the provider configurations the sign-in
writes. Every value here is authored for the corpus.
"""

from __future__ import annotations

from typing import Any


def auth_state_cases() -> list[dict[str, Any]]:
    """The five-source matrix: 2^4 boolean combinations per key kind, plus the
    empty key, empty-string values and unreadable sources."""

    cases: list[dict[str, Any]] = []
    for key_kind in ("default", "custom"):
        for had_before in (False, True):
            for process_env in (None, "oracle-process-value"):
                for keyring_value in (None, "oracle-keyring-value"):
                    for dotenv_state in ("absent", "value"):
                        label = (
                            f"{key_kind}"
                            f"-before-{'yes' if had_before else 'no'}"
                            f"-process-{'yes' if process_env else 'no'}"
                            f"-keyring-{'yes' if keyring_value else 'no'}"
                            f"-dotenv-{'yes' if dotenv_state == 'value' else 'no'}"
                        )
                        cases.append(
                            {
                                "case": label,
                                "envKeyKind": key_kind,
                                "hadValueBeforeDotenvLoad": had_before,
                                "processEnv": process_env,
                                "keyring": keyring_value,
                                "dotenv": dotenv_state,
                            }
                        )
    cases.append(
        {
            "case": "empty-env-key",
            "envKeyKind": "empty",
            "hadValueBeforeDotenvLoad": False,
            "processEnv": None,
            "keyring": None,
            "dotenv": "absent",
        }
    )
    edge_cases = [
        ("empty-string-in-process-env", {"processEnv": ""}),
        ("empty-string-in-keyring", {"keyring": ""}),
        ("empty-value-in-dotenv", {"dotenv": "empty"}),
        (
            "every-source-empty-string",
            {"processEnv": "", "keyring": "", "dotenv": "empty"},
        ),
        ("dotenv-path-is-a-directory", {"dotenv": "directory"}),
        (
            "dotenv-directory-with-keyring-value",
            {"dotenv": "directory", "keyring": "oracle-keyring-value"},
        ),
        ("dotenv-unreadable", {"dotenv": "unreadable"}),
        (
            "dotenv-unreadable-with-keyring-value",
            {"dotenv": "unreadable", "keyring": "oracle-keyring-value"},
        ),
    ]
    for label, overrides in edge_cases:
        base = {
            "case": label,
            "envKeyKind": "default",
            "hadValueBeforeDotenvLoad": False,
            "processEnv": None,
            "keyring": None,
            "dotenv": "absent",
        }
        base.update(overrides)
        cases.append(base)
    return cases


def service_cases() -> list[dict[str, Any]]:
    completed = {"status": "completed", "exchangeToken": "oracle-token"}
    pending = {"status": "pending"}
    failure = {"raise": "poll_failed"}
    return [
        {"case": "service-immediate-success", "polls": [completed]},
        {
            "case": "service-two-pending-polls-then-success",
            "polls": [pending, pending, completed],
        },
        {
            "case": "service-two-poll-failures-then-success",
            "polls": [failure, failure, completed],
        },
        {
            "case": "service-three-consecutive-poll-failures-stop",
            "polls": [failure, failure, failure],
        },
        {
            "case": "service-failure-streak-resets-on-success",
            "polls": [failure, failure, pending, failure, failure, completed],
        },
        {"case": "service-denied", "polls": [{"status": "denied"}]},
        {"case": "service-expired-status", "polls": [{"status": "expired"}]},
        {
            "case": "service-provider-error-with-default-message",
            "polls": [{"status": "error"}],
        },
        {
            "case": "service-provider-error-with-server-message",
            "polls": [{"status": "error", "message": "scripted server detail"}],
        },
        {"case": "service-unknown-status", "polls": [{"status": "half-done"}]},
        {
            "case": "service-completed-without-exchange-token",
            "polls": [{"status": "completed"}],
        },
        {
            "case": "service-expiry-clamps-the-sleep",
            "polls": [pending, pending, pending],
            "expiresIn": 4.0,
        },
        {
            "case": "service-already-expired-at-start",
            "polls": [],
            "expiresIn": -1.0,
        },
        {
            "case": "service-timeout-after-pending-polls",
            "polls": [pending, pending, pending],
            "expiresIn": 7.0,
        },
        {
            "case": "service-start-failure-opens-no-browser",
            "createError": "start_failed",
        },
        {
            "case": "service-open-browser-raises",
            "polls": [completed],
            "opener": "raise",
        },
        {
            "case": "service-open-browser-returns-false",
            "polls": [completed],
            "opener": "refuse",
        },
        {
            "case": "service-exchange-failure",
            "polls": [completed],
            "exchange": {"raise": "exchange_failed"},
        },
        {
            "case": "service-exchange-missing-api-key",
            "polls": [completed],
            "exchange": {"raise": "missing_api_key"},
        },
    ]


BROWSER_BASE = "https://console.mistral.ai"
API_BASE = "https://console.mistral.ai/api"
CUSTOM_BROWSER_BASE = "https://console.internal.example"
CUSTOM_API_BASE = "https://console.internal.example/api"

SPLIT_API_BASE = "https://connector.internal.example:8443/api"
PUBLIC_CREATE = {
    "process_id": "oracle-process",
    "sign_in_url": "https://console.public.example/oracle/sign-in?x=1#y",
    "poll_url": "https://console.public.example/api/oracle/poll",
    "expires_at": "2026-01-01T00:10:00Z",
}

WELL_FORMED_CREATE = {
    "process_id": "oracle-process",
    "sign_in_url": f"{BROWSER_BASE}/oracle/sign-in",
    "poll_url": f"{API_BASE}/oracle/poll",
    "expires_at": "2026-01-01T00:10:00Z",
}


def gateway_cases() -> list[dict[str, Any]]:
    return [
        {
            "case": "gateway-create-posts-the-pkce-payload",
            "op": "create",
            "responses": [{"body": WELL_FORMED_CREATE}],
        },
        {
            "case": "gateway-create-normalizes-the-zulu-expiry",
            "op": "create",
            "responses": [
                {
                    "body": {
                        **WELL_FORMED_CREATE,
                        "expires_at": "2026-01-01T01:30:00+01:00",
                    }
                }
            ],
        },
        {
            "case": "gateway-create-follows-a-custom-api-base",
            "op": "create",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": CUSTOM_API_BASE,
            "responses": [
                {
                    "body": {
                        "process_id": "oracle-process",
                        "sign_in_url": f"{CUSTOM_BROWSER_BASE}/oracle/sign-in",
                        "poll_url": f"{CUSTOM_API_BASE}/oracle/poll",
                        "expires_at": "2026-01-01T00:10:00Z",
                    }
                }
            ],
        },
        {
            "case": "gateway-create-transport-error",
            "op": "create",
            "responses": [{"transportError": True}],
        },
        {
            "case": "gateway-create-non-success-status",
            "op": "create",
            "responses": [{"status": 500, "body": {}}],
        },
        {
            "case": "gateway-create-unparseable-body",
            "op": "create",
            "responses": [{"rawBody": "not json"}],
        },
        {
            "case": "gateway-create-missing-field",
            "op": "create",
            "responses": [{"body": {"process_id": "oracle-process"}}],
        },
        {
            "case": "gateway-create-rejects-off-origin-sign-in-url",
            "op": "create",
            "responses": [
                {
                    "body": {
                        **WELL_FORMED_CREATE,
                        "sign_in_url": "https://evil.example/oracle/sign-in",
                    }
                }
            ],
        },
        {
            "case": "gateway-create-rejects-off-origin-poll-url",
            "op": "create",
            "responses": [
                {
                    "body": {
                        **WELL_FORMED_CREATE,
                        "poll_url": "https://evil.example/api/oracle/poll",
                    }
                }
            ],
        },
        {
            "case": "gateway-poll-reads-a-pending-status",
            "op": "poll",
            "responses": [{"body": {"status": "pending"}}],
        },
        {
            "case": "gateway-poll-reads-a-completed-status",
            "op": "poll",
            "responses": [
                {
                    "body": {
                        "status": "completed",
                        "exchange_token": "oracle-token",
                    }
                }
            ],
        },
        {
            "case": "gateway-poll-maps-410-to-expired",
            "op": "poll",
            "responses": [{"status": 410, "body": {}}],
        },
        {
            "case": "gateway-poll-transport-error",
            "op": "poll",
            "responses": [{"transportError": True}],
        },
        {
            "case": "gateway-poll-non-success-status",
            "op": "poll",
            "responses": [{"status": 503, "body": {}}],
        },
        {
            "case": "gateway-poll-unparseable-body",
            "op": "poll",
            "responses": [{"rawBody": "<html>"}],
        },
        {
            "case": "gateway-poll-unknown-status",
            "op": "poll",
            "responses": [{"body": {"status": "half-done"}}],
        },
        {
            "case": "gateway-poll-revalidates-before-requesting",
            "op": "poll",
            "pollUrl": "https://evil.example/api/oracle/poll",
            "responses": [],
        },
        {
            "case": "gateway-exchange-posts-the-token-and-verifier",
            "op": "exchange",
            "responses": [{"body": {"api_key": "oracle-key"}}],
        },
        {
            "case": "gateway-exchange-success-without-a-key",
            "op": "exchange",
            "responses": [{"body": {"detail": "scripted-detail"}}],
        },
        {
            "case": "gateway-exchange-transport-error",
            "op": "exchange",
            "responses": [{"transportError": True}],
        },
        {
            "case": "gateway-exchange-non-success-status",
            "op": "exchange",
            "responses": [{"status": 403, "body": {"detail": "scripted-denial"}}],
        },
        {
            "case": "gateway-exchange-unparseable-body",
            "op": "exchange",
            "responses": [{"rawBody": ""}],
        },
        {
            "case": "gateway-create-rehomes-split-horizon-urls",
            "op": "create",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": SPLIT_API_BASE,
            "allowOriginRewrite": True,
            "responses": [{"body": PUBLIC_CREATE}],
        },
        {
            "case": "gateway-create-refuses-foreign-urls-without-the-rewrite",
            "op": "create",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": SPLIT_API_BASE,
            "responses": [{"body": PUBLIC_CREATE}],
        },
        {
            "case": "gateway-create-rewrite-still-holds-the-path",
            "op": "create",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": SPLIT_API_BASE,
            "allowOriginRewrite": True,
            "responses": [
                {
                    "body": {
                        **PUBLIC_CREATE,
                        "poll_url": "https://console.public.example/elsewhere/poll",
                    }
                }
            ],
        },
        {
            "case": "gateway-poll-rehomes-a-foreign-poll-url",
            "op": "poll",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": SPLIT_API_BASE,
            "allowOriginRewrite": True,
            "pollUrl": "https://console.public.example/api/oracle/poll?attempt=1",
            "responses": [{"body": {"status": "pending"}}],
        },
        {
            "case": "gateway-exchange-goes-to-the-configured-api-base",
            "op": "exchange",
            "browserBase": CUSTOM_BROWSER_BASE,
            "apiBase": SPLIT_API_BASE,
            "allowOriginRewrite": True,
            "responses": [{"body": {"api_key": "oracle-api-key"}}],
        },
    ]


def url_validation_cases() -> list[dict[str, Any]]:
    base = "https://console.mistral.ai/api"
    bare = "https://console.mistral.ai"
    return [
        {"case": "exact-base", "value": base, "base": base},
        {"case": "path-under-base", "value": f"{base}/poll/1", "base": base},
        {"case": "deeper-path-under-base", "value": f"{base}/a/b/c", "base": base},
        {"case": "different-host", "value": "https://evil.example/api/x", "base": base},
        {"case": "different-subdomain", "value": "https://api.mistral.ai/api/x", "base": base},
        {"case": "different-scheme", "value": "http://console.mistral.ai/api/x", "base": base},
        {
            "case": "explicit-default-https-port-on-value",
            "value": "https://console.mistral.ai:443/api/x",
            "base": base,
        },
        {
            "case": "explicit-default-https-port-on-base",
            "value": f"{base}/x",
            "base": "https://console.mistral.ai:443/api",
        },
        {
            "case": "explicit-default-http-ports-both-sides",
            "value": "http://gateway.local:80/api/x",
            "base": "http://gateway.local/api",
        },
        {
            "case": "custom-port-matching",
            "value": "http://localhost:8080/api/x",
            "base": "http://localhost:8080/api",
        },
        {
            "case": "custom-port-mismatch",
            "value": "http://localhost:9090/api/x",
            "base": "http://localhost:8080/api",
        },
        {
            "case": "default-port-against-custom-port",
            "value": "https://console.mistral.ai/api/x",
            "base": "https://console.mistral.ai:8443/api",
        },
        {"case": "bare-origin-base-accepts-any-path", "value": f"{bare}/anywhere", "base": bare},
        {"case": "bare-origin-base-accepts-root", "value": bare, "base": bare},
        {
            "case": "textual-prefix-not-a-segment-boundary",
            "value": "https://console.mistral.ai/apix/poll",
            "base": base,
        },
        {
            "case": "base-with-trailing-slash-path",
            "value": f"{base}/poll",
            "base": f"{base}/",
        },
        {
            "case": "dot-segments-escaping-the-base",
            "value": "https://console.mistral.ai/api/../admin",
            "base": base,
        },
        {
            "case": "dot-segments-staying-inside",
            "value": "https://console.mistral.ai/api/v1/../v2",
            "base": base,
        },
        {
            "case": "percent-encoded-dot-segments-escaping",
            "value": "https://console.mistral.ai/api/%2e%2e/admin",
            "base": base,
        },
        {
            "case": "percent-encoded-dot-segments-inside",
            "value": "https://console.mistral.ai/api/%2e%2e/api/poll",
            "base": base,
        },
        {
            "case": "double-encoded-dot-segments",
            "value": "https://console.mistral.ai/api/%252e%252e/admin",
            "base": base,
        },
        {
            "case": "current-directory-dot-segment",
            "value": "https://console.mistral.ai/api/./poll",
            "base": base,
        },
        {"case": "query-string-is-ignored", "value": f"{base}/poll?token=x", "base": base},
        {"case": "fragment-is-ignored", "value": f"{base}/poll#frag", "base": base},
        {
            "case": "userinfo-does-not-change-the-origin",
            "value": "https://user:pass@console.mistral.ai/api/poll",
            "base": base,
        },
        {"case": "uppercase-scheme", "value": "HTTPS://console.mistral.ai/api/poll", "base": base},
        {"case": "uppercase-host", "value": "https://CONSOLE.MISTRAL.AI/api/poll", "base": base},
        {
            "case": "invalid-port-on-the-value",
            "value": "https://console.mistral.ai:port/api/poll",
            "base": base,
        },
        {"case": "relative-url", "value": "/api/poll", "base": base},
        {"case": "schemeless-url", "value": "console.mistral.ai/api/poll", "base": base},
        {"case": "non-http-scheme", "value": "file://console.mistral.ai/api/poll", "base": base},
        {
            "case": "ip-host-matching",
            "value": "http://127.0.0.1:8080/api/poll",
            "base": "http://127.0.0.1:8080/api",
        },
        {
            "case": "ip-host-mismatch",
            "value": "http://127.0.0.2:8080/api/poll",
            "base": "http://127.0.0.1:8080/api",
        },
    ]


def url_rewrite_cases() -> list[dict[str, Any]]:
    """Every validation case again, plus the shapes only a re-homing gateway
    meets: a foreign origin under the base path, a base carrying its own port
    or credentials, and values a WHATWG parser would normalize."""

    connector = "https://connector.internal.example:8443/api"
    public = "https://console.public.example"
    extra = [
        ("foreign-origin-rehomed", f"{public}/api/vibe/sign-in/1", connector),
        ("foreign-origin-outside-the-base-path", f"{public}/other/1", connector),
        ("foreign-origin-onto-a-bare-base", f"{public}/any/path?q=1", "https://connector.internal.example"),
        ("base-default-port-kept-verbatim", f"{public}/api/poll", "https://connector.internal.example:443/api"),
        ("base-credentials-spliced-in", f"{public}/api/poll", "https://user:secret@connector.internal.example/api"),
        ("value-credentials-dropped", "https://user:pw@console.public.example/api/poll", connector),
        ("value-dot-segment-kept-verbatim", f"{public}/api/./poll?x=1#f", connector),
        ("value-encoding-kept-verbatim", f"{public}/api/a%20b/%7Euser", connector),
        ("value-empty-path-under-bare-base", public, "https://connector.internal.example"),
        ("value-upper-case-host", "https://CONSOLE.PUBLIC.EXAMPLE/api/poll", connector),
        ("relative-value-rehomed", "/api/poll?x=1", connector),
        ("schemeless-value-stays-outside", "console.public.example/api/poll", connector),
        ("network-path-value-rehomed", "//console.public.example/api/poll", connector),
        ("custom-scheme-value-rehomed", "custom://console.public.example/api/poll", connector),
        ("file-scheme-value-rehomed", "file:///api/poll", connector),
        ("http-base-for-an-https-value", f"{public}/api/x", "http://gateway.local:8080/api"),
        ("ipv6-base", f"{public}/api/x", "https://[::1]:8443/api"),
        ("ipv6-value", "https://[2001:db8::1]/api/x", connector),
        ("bad-ipv6-value", "https://[::1/api/x", connector),
        ("bad-port-on-the-base", f"{public}/api/x", "https://connector.internal.example:port/api"),
        ("encoded-escape-under-rewrite", f"{public}/api/%2e%2e/admin", connector),
        ("tab-and-newline-in-the-value", f"{public}/api/\tpo\nll", connector),
        ("leading-space-in-the-value", f"  {public}/api/poll", connector),
    ]
    return [
        *({"case": case["case"], "value": case["value"], "base": case["base"]} for case in url_validation_cases()),
        *({"case": name, "value": value, "base": base} for name, value, base in extra),
    ]


def tenant_sanitize_cases() -> list[str]:
    return [
        "https://api.tenant.example",
        "https://api.tenant.example/",
        "https://api.tenant.example///",
        "https://gateway.tenant.example/mistral",
        "HTTPS://API.TENANT.EXAMPLE",
        "https://user:pw@api.tenant.example:8443/",
        "http://api.tenant.example",
        "ftp://api.tenant.example",
        "api.tenant.example",
        "//api.tenant.example",
        "https://",
        "https:///path",
        "",
        "https://api.tenant.example/../evil",
        "https://api.tenant.example/a..b",
        "https://api.tenant.example/x;..",
        "https://api.tenant.example?q=..",
        "https://api.tenant.example#..",
        "https://[::1",
        "https://[::1]:8443/v",
        " https://api.tenant.example",
    ]


#: Each answer the console gives, as a status and a body; ``rawBody`` is a
#: body that is not JSON.
TENANT_ANSWERS: list[dict[str, Any]] = [
    {"case": "unreachable-console", "status": 503, "body": {}},
    {"case": "refused-credential", "status": 401, "body": {}},
    {"case": "forbidden-credential", "status": 403, "body": {}},
    {"case": "no-hosts", "status": 200, "body": {"plan_type": "api", "plan_name": "x"}},
    {
        "case": "api-host-only",
        "status": 200,
        "body": {"plan_type": "api", "plan_name": "x", "api_base": "https://api.tenant.example/"},
    },
    {
        "case": "chat-host-only",
        "status": 200,
        "body": {"plan_type": "chat", "plan_name": "TEAM", "vibe_base": "https://chat.tenant.example"},
    },
    {
        "case": "both-hosts",
        "status": 200,
        "body": {
            "plan_type": "mistral_code",
            "plan_name": "E",
            "api_base": "https://gateway.tenant.example/mistral",
            "vibe_base": "https://chat.tenant.example/",
        },
    },
    {
        "case": "unsafe-api-host-safe-chat-host",
        "status": 200,
        "body": {
            "plan_type": "api",
            "plan_name": "x",
            "api_base": "http://api.tenant.example",
            "vibe_base": "https://chat.tenant.example",
        },
    },
    {
        "case": "empty-hosts",
        "status": 200,
        "body": {"plan_type": "api", "plan_name": "x", "api_base": "", "vibe_base": ""},
    },
    {
        "case": "null-hosts",
        "status": 200,
        "body": {"plan_type": "api", "plan_name": "x", "api_base": None, "vibe_base": None},
    },
    {
        "case": "host-of-the-wrong-type",
        "status": 200,
        "body": {"plan_type": "api", "plan_name": "x", "api_base": 5},
    },
    {
        "case": "unknown-plan-type",
        "status": 200,
        "body": {"plan_type": "enterprise", "plan_name": "x", "api_base": "https://api.tenant.example"},
    },
    {
        "case": "mixed-case-plan-type",
        "status": 200,
        "body": {"plan_type": " API ", "plan_name": "x", "api_base": "https://api.tenant.example"},
    },
    {
        "case": "extra-fields-ignored",
        "status": 200,
        "body": {"plan_type": "api", "plan_name": "x", "unknown": [1], "api_base": "https://api.tenant.example"},
    },
    {"case": "body-not-json", "status": 200, "rawBody": "<html>"},
    {"case": "body-not-an-object", "status": 200, "body": ["api_base"]},
]


def provider_credentials_cases() -> list[dict[str, Any]]:
    custom = {
        "browser_auth_base_url": "https://console.internal.example",
        "browser_auth_api_base_url": "https://console.internal.example/api",
    }
    split = {
        "browser_auth_base_url": "https://console.internal.example",
        "browser_auth_api_base_url": "https://connector.internal.example/api",
        "browser_auth_allow_origin_rewrite": True,
    }
    return [
        {
            "case": "fresh-home-custom-console",
            "user": None,
            "provider": custom,
            "consoleBaseUrl": "https://console.internal.example",
        },
        {
            "case": "fresh-home-split-horizon-with-tenant-hosts",
            "user": None,
            "provider": {**split, "api_base": "https://api.tenant.example/v1"},
            "consoleBaseUrl": "https://connector.internal.example",
            "vibeBaseUrl": "https://chat.tenant.example",
        },
        {
            "case": "replaces-the-configured-entry",
            "user": (
                'theme = "dark"\n'
                'console_base_url = "https://old.example"\n\n'
                "[[providers]]\n"
                'name = "mistral"\n'
                'api_base = "https://api.old.example/v1"\n'
                'api_key_env_var = "MISTRAL_API_KEY"\n'
                'browser_auth_base_url = "https://old.example"\n'
                "browser_auth_allow_origin_rewrite = true\n"
                'backend = "mistral"\n'
                'custom_note = "kept or dropped"\n\n'
                "[[providers]]\n"
                'name = "other"\n'
                'api_base = "https://other.example/v1"\n'
                'other_note = "kept or dropped"\n'
            ),
            "provider": custom,
            "consoleBaseUrl": "https://console.internal.example",
        },
        {
            "case": "provider-only",
            "user": 'console_base_url = "https://console.internal.example"\n',
            "provider": custom,
        },
        {
            "case": "default-provider-with-moved-bases",
            "user": None,
            "provider": {},
            "consoleBaseUrl": "https://console.mistral.ai",
            "vibeBaseUrl": "https://chat.mistral.ai",
        },
        {
            "case": "keeps-unrelated-tables",
            "user": (
                'active_model = "devstral"\n\n'
                "[tools.bash]\n"
                'permission = "always"\n'
            ),
            "provider": split,
            "vibeBaseUrl": "https://chat.tenant.example",
        },
    ]


def tenant_reconcile_cases() -> list[dict[str, Any]]:
    """Each case: the user file, the provider the account read used, and the
    ``/whoami`` hosts the console answered."""

    tenant_provider = (
        "[[providers]]\n"
        'name = "mistral"\n'
        'api_base = "https://api.tenant.example/v1"\n'
        'api_key_env_var = "MISTRAL_API_KEY"\n'
        'browser_auth_base_url = "https://console.internal.example"\n'
        'backend = "mistral"\n'
    )
    return [
        {"case": "no-hosts-writes-nothing", "user": None, "providerName": "mistral", "whoami": {}},
        {
            "case": "a-new-api-host-moves-the-provider",
            "user": None,
            "providerName": "mistral",
            "whoami": {"api_base": "https://api.tenant.example/"},
        },
        {
            "case": "the-configured-api-host-writes-nothing",
            "user": tenant_provider,
            "providerName": "mistral",
            "whoami": {"api_base": "https://api.tenant.example"},
        },
        {
            "case": "a-moved-api-host-replaces-the-entry",
            "user": 'theme = "dark"\n\n' + tenant_provider + 'custom_note = "dropped"\n',
            "providerName": "mistral",
            "whoami": {"api_base": "https://gateway.tenant.example/mistral"},
        },
        {
            "case": "a-new-chat-host-moves-the-chat-base",
            "user": None,
            "providerName": "mistral",
            "whoami": {"vibe_base": "https://chat.tenant.example/"},
        },
        {
            "case": "the-configured-chat-host-writes-nothing",
            "user": 'vibe_base_url = "https://chat.tenant.example"\n',
            "providerName": "mistral",
            "whoami": {"vibe_base": "https://chat.tenant.example"},
        },
        {
            "case": "both-hosts-move-together",
            "user": 'theme = "dark"\n',
            "providerName": "mistral",
            "whoami": {
                "api_base": "https://api.tenant.example",
                "vibe_base": "https://chat.tenant.example",
            },
        },
        {
            "case": "an-unsafe-api-host-is-refused",
            "user": None,
            "providerName": "mistral",
            "whoami": {
                "api_base": "http://api.tenant.example",
                "vibe_base": "https://chat.tenant.example",
            },
        },
        {
            "case": "empty-hosts-write-nothing",
            "user": None,
            "providerName": "mistral",
            "whoami": {"api_base": "", "vibe_base": ""},
        },
        {
            "case": "an-unknown-provider-keeps-its-api-base",
            "user": None,
            "providerName": "other",
            "whoami": {"api_base": "https://api.tenant.example"},
        },
        {
            "case": "a-generic-provider-of-that-name-is-skipped",
            "user": (
                "[[providers]]\n"
                'name = "gateway"\n'
                'api_base = "https://llm.example/v1"\n'
                'api_key_env_var = "GATEWAY_KEY"\n'
            ),
            "providerName": "gateway",
            "whoami": {"api_base": "https://api.tenant.example"},
        },
    ]


ACP_CUSTOM = {
    "browser_auth_base_url": "https://console.internal.example",
    "browser_auth_api_base_url": "https://console.internal.example/api",
}
ACP_SPLIT = {
    "browser_auth_base_url": "https://console.internal.example",
    "browser_auth_api_base_url": "https://connector.internal.example:8443/api",
    "browser_auth_allow_origin_rewrite": True,
}
ACP_PUBLIC_SPLIT = {
    "browser_auth_api_base_url": "https://connector.internal.example/api",
    "browser_auth_allow_origin_rewrite": True,
}
ACP_TENANT = {
    "api_base": "https://api.tenant.example",
    "vibe_base": "https://chat.tenant.example",
}


def acp_sign_in_cases() -> list[dict[str, Any]]:
    """Each case: the configured provider (overrides on the shipped Mistral
    entry, or ``generic`` for a provider without browser sign-in), the
    configured bases, the ``authenticate`` arguments, the ``/whoami`` answer,
    and what the scripted persisters report."""

    def case(name: str, **fields: Any) -> dict[str, Any]:
        return {
            "case": name,
            "provider": fields.pop("provider", {}),
            "consoleBaseUrl": fields.pop("consoleBaseUrl", "https://console.mistral.ai"),
            "vibeBaseUrl": fields.pop("vibeBaseUrl", "https://chat.mistral.ai"),
            "arguments": fields.pop("arguments", {}),
            "whoami": fields.pop("whoami", None),
            "apiKeyResult": fields.pop("apiKeyResult", "completed"),
            "providerOk": fields.pop("providerOk", True),
            "consoleOk": fields.pop("consoleOk", True),
            "vibeOk": fields.pop("vibeOk", True),
            **fields,
        }

    custom = lambda **extra: {"signInTarget": "custom", **extra}  # noqa: E731
    return [
        case("no-target-default-provider-skips-the-batch"),
        case(
            "no-target-custom-provider-with-aligned-console-skips",
            provider=ACP_CUSTOM,
            consoleBaseUrl="https://console.internal.example",
        ),
        case(
            "no-target-custom-provider-aligns-the-console",
            provider=ACP_CUSTOM,
            whoami=ACP_TENANT,
        ),
        case(
            "no-target-public-split-aligns-to-the-connector",
            provider=ACP_PUBLIC_SPLIT,
            whoami={"vibe_base": "https://chat.tenant.example"},
        ),
        case(
            "no-target-split-provider-on-the-connector-skips",
            provider=ACP_SPLIT,
            consoleBaseUrl="https://connector.internal.example:8443",
        ),
        case(
            "mistral-target-on-the-default-provider-skips",
            arguments={"signInTarget": "mistral"},
        ),
        case(
            "mistral-target-resets-a-custom-provider",
            provider=ACP_SPLIT,
            consoleBaseUrl="https://connector.internal.example:8443",
            vibeBaseUrl="https://chat.tenant.example",
            arguments={"signInTarget": "mistral"},
        ),
        case(
            "null-target-is-no-target",
            arguments={"signInTarget": None},
        ),
        case(
            "custom-target-derives-the-api-base",
            arguments=custom(domain="https://console.internal.example"),
        ),
        case(
            "custom-target-adopts-the-tenant-hosts",
            arguments=custom(domain="https://console.internal.example/"),
            whoami=ACP_TENANT,
        ),
        case(
            "custom-target-split-api-base-turns-the-rewrite-on",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="https://connector.internal.example:8443/api",
            ),
            whoami=ACP_TENANT,
        ),
        case(
            "custom-target-same-origin-api-base-keeps-the-rewrite-off",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="https://console.internal.example/connector/api",
            ),
        ),
        case(
            "custom-target-strips-the-api-base",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="  https://connector.internal.example/api  ",
            ),
        ),
        case(
            "custom-target-blank-api-base-derives",
            arguments=custom(domain="https://console.internal.example", apiBaseUrl="   "),
        ),
        case(
            "custom-target-null-api-base-derives",
            arguments=custom(domain="https://console.internal.example", apiBaseUrl=None),
        ),
        case(
            "custom-target-non-string-api-base-is-refused",
            arguments=custom(domain="https://console.internal.example", apiBaseUrl=8443),
        ),
        case(
            "custom-target-local-http-api-base-is-kept",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="http://localhost:8080/api",
            ),
        ),
        case(
            "custom-target-schemeless-api-base-gains-https",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="connector.internal.example/api",
            ),
        ),
        case(
            "custom-target-invalid-api-base-is-refused",
            arguments=custom(
                domain="https://console.internal.example",
                apiBaseUrl="https:/connector.internal.example",
            ),
        ),
        case(
            "custom-target-schemeless-domain-gains-https",
            arguments=custom(domain="console.internal.example"),
        ),
        case(
            "custom-target-invalid-domain-is-refused",
            arguments=custom(domain="https:/console.internal.example"),
        ),
        case(
            "custom-target-blank-domain-is-refused",
            arguments=custom(domain="   "),
        ),
        case("custom-target-missing-domain-is-refused", arguments=custom()),
        case(
            "custom-target-non-string-domain-is-refused",
            arguments=custom(domain=["https://console.internal.example"]),
        ),
        case("unknown-target-is-refused", arguments={"signInTarget": "other"}),
        case("non-string-target-is-refused", arguments={"signInTarget": 7}),
        case("unsupported-action-is-refused", arguments={"action": "complete"}),
        case("generic-provider-is-refused", provider="generic"),
        case(
            "the-tenant-repeats-the-configured-chat-host",
            arguments=custom(domain="https://console.internal.example"),
            vibeBaseUrl="https://chat.tenant.example",
            whoami=ACP_TENANT,
        ),
        case(
            "a-failed-key-save-still-writes-the-provider",
            arguments=custom(domain="https://console.internal.example"),
            apiKeyResult="save_error:disk full",
        ),
        case(
            "each-failed-write-is-reported",
            arguments=custom(domain="https://console.internal.example"),
            whoami=ACP_TENANT,
            providerOk=False,
            consoleOk=False,
            vibeOk=False,
        ),
    ]
