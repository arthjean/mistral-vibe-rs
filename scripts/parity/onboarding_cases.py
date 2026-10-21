"""The custom-domain inputs ``onboarding.py`` hands the reference's validators.

Every domain and API base here is authored for the corpus; none is read from
the reference.
"""

from __future__ import annotations


def domain_cases() -> list[str]:
    return [
        "console.mistral.ai",
        "https://console.mistral.ai",
        "https://console.mistral.ai/",
        "console.internal.example",
        "https://console.internal.example",
        "http://localhost:8080",
        "localhost:8080",
        "console.oracle.mistral.ai",
        "https://console.oracle.mistral.ai",
        "console.mistral.ai.evil.example",
        "foo.mistral.ai",
        "console.example.com",
        "example.com/console",
        "example.com:8443",
        "192.168.1.10",
        "192.168.1.10:8080",
        "https://user:pass@example.com",
        "",
        "   ",
        "http:/broken",
        "not a domain",
        "ftp://example.com",
        "https://",
        "https://.",
        "console..mistral.ai",
        "münchen.example",
    ]


def split_api_bases(domain: str) -> list[str]:
    """The API bases each valid domain is paired with: none, a foreign host, the
    domain's own host spelled differently, and the domain itself."""

    return [
        "",
        "connector.internal.example/api",
        "https://connector.internal.example:8443/api/",
        "HTTPS://CONSOLE.MISTRAL.AI:443/api",
        "http://console.mistral.ai/api",
        "localhost:8080/api",
        domain,
    ]
