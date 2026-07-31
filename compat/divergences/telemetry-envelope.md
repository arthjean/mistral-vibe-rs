# Telemetry envelope safety divergence

The 2.23.1 upstream client sends an unversioned envelope with event properties
merged into one open dictionary. That surface can represent prompts, full
paths, exception strings, proxy credentials, and tool output.

Mistral Vibe RS preserves the registered event names, correlation semantics,
Mistral-credential eligibility, endpoint selection, and opt-out behavior. It
intentionally emits schema version 1 with a nested typed metadata block and a
closed set of bounded scalar attributes. Arbitrary strings, filesystem paths,
URLs, exception text, prompts, file content, and tool output cannot enter the
telemetry type.

Disabled telemetry and sessions without an eligible Mistral credential create
neither a request nor a persistent queue entry. Credentials are sent only to
the HTTPS Mistral event endpoint and never appear in payloads or errors.
