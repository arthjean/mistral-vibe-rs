"""The configuration documents ``voice.py`` resolves through the pinned reference.

Every alias, model, provider, host and variable here is authored for the corpus;
none is read from the reference.
"""

from __future__ import annotations

#: The configuration documents every family is measured over, as the user layer
#: merged over the shipped defaults. Every alias, model name, provider name,
#: host and variable here is authored for this corpus.
#:
#: The union merges key `transcribe_models` and `tts_models` by `alias` and the
#: provider lists by `name`, so an entry naming a shipped alias amends it and an
#: entry naming a new one is appended.
DOCUMENTS: list[dict[str, str]] = [
    {"id": "defaults", "toml": ""},
    {
        "id": "transcribe-model-name",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
name = "fixture-transcribe-model"
provider = "mistral"
""",
    },
    {
        "id": "transcribe-sample-rate",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
name = "fixture-rate-model"
provider = "mistral"
sample_rate = 48000
""",
    },
    {
        "id": "transcribe-language",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
name = "fixture-language-model"
provider = "mistral"
language = "fr"
""",
    },
    {
        "id": "transcribe-streaming-delay",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
name = "fixture-delay-model"
provider = "mistral"
target_streaming_delay_ms = 1200
""",
    },
    {
        "id": "transcribe-second-model-active",
        "toml": """
active_transcribe_model = "fixture-alias"

[[transcribe_models]]
name = "fixture-second-model"
provider = "mistral"
alias = "fixture-alias"
sample_rate = 24000
encoding = "pcm_s16le"
language = "de"
target_streaming_delay_ms = 250
""",
    },
    {
        "id": "transcribe-second-model-inactive",
        "toml": """
[[transcribe_models]]
name = "fixture-unused-model"
provider = "mistral"
alias = "fixture-unused"
sample_rate = 22050
""",
    },
    {
        "id": "transcribe-entry-without-optional-fields",
        "toml": """
active_transcribe_model = "fixture-bare"

[[transcribe_models]]
name = "fixture-bare-model"
provider = "mistral"
alias = "fixture-bare"
""",
    },
    {
        "id": "transcribe-partial-entry-for-shipped-alias",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
sample_rate = 48000
""",
    },
    {
        "id": "transcribe-active-alias-undeclared",
        "toml": 'active_transcribe_model = "fixture-absent"\n',
    },
    {
        "id": "transcribe-provider-undeclared",
        "toml": """
active_transcribe_model = "fixture-orphan"

[[transcribe_models]]
name = "fixture-orphan-model"
provider = "fixture-absent-provider"
alias = "fixture-orphan"
""",
    },
    {
        "id": "transcribe-gateway-provider",
        "toml": """
active_transcribe_model = "fixture-gateway"

[[transcribe_providers]]
name = "fixture-gateway-provider"
api_base = "wss://gateway.fixture.invalid:8443"
api_key_env_var = "FIXTURE_GATEWAY_TOKEN"

[[transcribe_models]]
name = "fixture-gateway-model"
provider = "fixture-gateway-provider"
alias = "fixture-gateway"
sample_rate = 8000
target_streaming_delay_ms = 750
""",
    },
    {
        "id": "transcribe-http-provider",
        "toml": """
active_transcribe_model = "fixture-local"

[[transcribe_providers]]
name = "fixture-local-provider"
api_base = "http://localhost:8080"
api_key_env_var = "FIXTURE_LOCAL_TOKEN"

[[transcribe_models]]
name = "fixture-local-model"
provider = "fixture-local-provider"
alias = "fixture-local"
""",
    },
    {
        "id": "transcribe-provider-path-suffix",
        "toml": """
active_transcribe_model = "fixture-suffixed"

[[transcribe_providers]]
name = "fixture-suffixed-provider"
api_base = "https://gateway.fixture.invalid/audio/"
api_key_env_var = "FIXTURE_GATEWAY_TOKEN"

[[transcribe_models]]
name = "fixture-suffixed-model"
provider = "fixture-suffixed-provider"
alias = "fixture-suffixed"
""",
    },
    {
        "id": "transcribe-provider-without-credential",
        "toml": """
active_transcribe_model = "fixture-anonymous"

[[transcribe_providers]]
name = "fixture-anonymous-provider"
api_base = "wss://anonymous.fixture.invalid"
api_key_env_var = ""

[[transcribe_models]]
name = "fixture-anonymous-model"
provider = "fixture-anonymous-provider"
alias = "fixture-anonymous"
""",
    },
    {
        "id": "transcribe-entry-without-alias",
        "toml": """
[[transcribe_models]]
name = "fixture-unaliased-model"
provider = "mistral"
""",
    },
    {
        "id": "transcribe-duplicate-alias",
        "toml": """
active_transcribe_model = "fixture-twice"

[[transcribe_models]]
name = "fixture-first"
provider = "mistral"
alias = "fixture-twice"
sample_rate = 32000

[[transcribe_models]]
name = "fixture-second"
provider = "mistral"
alias = "fixture-twice"
language = "es"
""",
    },
    {
        "id": "transcribe-unsupported-encoding",
        "toml": """
[[transcribe_models]]
alias = "voxtral-realtime"
name = "fixture-encoded-model"
provider = "mistral"
encoding = "fixture-encoding"
""",
    },
    {"id": "speech-defaults-with-narrator", "toml": "narrator_enabled = true\n"},
    {
        "id": "speech-model-name",
        "toml": """
[[tts_models]]
alias = "voxtral-tts"
name = "fixture-speech-model"
provider = "mistral"
""",
    },
    {
        "id": "speech-voice",
        "toml": """
[[tts_models]]
alias = "voxtral-tts"
name = "fixture-voiced-model"
provider = "mistral"
voice = "fixture-voice"
""",
    },
    {
        "id": "speech-response-format",
        "toml": """
[[tts_models]]
alias = "voxtral-tts"
name = "fixture-format-model"
provider = "mistral"
response_format = "mp3"
""",
    },
    {
        "id": "speech-second-model-active",
        "toml": """
active_tts_model = "fixture-speech-alias"

[[tts_models]]
name = "fixture-second-speech-model"
provider = "mistral"
alias = "fixture-speech-alias"
voice = "fixture-second-voice"
response_format = "flac"
""",
    },
    {
        "id": "speech-entry-without-optional-fields",
        "toml": """
active_tts_model = "fixture-bare-speech"

[[tts_models]]
name = "fixture-bare-speech-model"
provider = "mistral"
alias = "fixture-bare-speech"
""",
    },
    {
        "id": "speech-active-alias-undeclared",
        "toml": 'active_tts_model = "fixture-absent-speech"\n',
    },
    {
        "id": "speech-provider-undeclared",
        "toml": """
active_tts_model = "fixture-orphan-speech"

[[tts_models]]
name = "fixture-orphan-speech-model"
provider = "fixture-absent-speech-provider"
alias = "fixture-orphan-speech"
""",
    },
    {
        "id": "speech-gateway-provider",
        "toml": """
active_tts_model = "fixture-speech-gateway"

[[tts_providers]]
name = "fixture-speech-gateway-provider"
api_base = "https://speech.fixture.invalid:9443"
api_key_env_var = "FIXTURE_SPEECH_TOKEN"

[[tts_models]]
name = "fixture-speech-gateway-model"
provider = "fixture-speech-gateway-provider"
alias = "fixture-speech-gateway"
voice = "fixture-gateway-voice"
response_format = "pcm"
""",
    },
    {
        "id": "speech-provider-without-credential",
        "toml": """
active_tts_model = "fixture-anonymous-speech"

[[tts_providers]]
name = "fixture-anonymous-speech-provider"
api_base = "https://anonymous.fixture.invalid"
api_key_env_var = ""

[[tts_models]]
name = "fixture-anonymous-speech-model"
provider = "fixture-anonymous-speech-provider"
alias = "fixture-anonymous-speech"
""",
    },
    {
        "id": "speech-entry-without-alias",
        "toml": """
[[tts_models]]
name = "fixture-unaliased-speech-model"
provider = "mistral"
""",
    },
    {
        "id": "speech-duplicate-alias",
        "toml": """
active_tts_model = "fixture-speech-twice"

[[tts_models]]
name = "fixture-speech-first"
provider = "mistral"
alias = "fixture-speech-twice"
voice = "fixture-first-voice"

[[tts_models]]
name = "fixture-speech-second"
provider = "mistral"
alias = "fixture-speech-twice"
response_format = "opus"
""",
    },
    {
        "id": "speech-unsupported-response-format",
        "toml": """
[[tts_models]]
alias = "voxtral-tts"
name = "fixture-unsupported-model"
provider = "mistral"
response_format = "fixture-format"
""",
    },
    {
        "id": "both-surfaces-on-one-gateway",
        "toml": """
voice_mode_enabled = true
narrator_enabled = true
active_transcribe_model = "fixture-both-in"
active_tts_model = "fixture-both-out"

[[transcribe_providers]]
name = "fixture-both-provider"
api_base = "wss://both.fixture.invalid"
api_key_env_var = "FIXTURE_BOTH_TOKEN"

[[tts_providers]]
name = "fixture-both-provider"
api_base = "https://both.fixture.invalid"
api_key_env_var = "FIXTURE_BOTH_TOKEN"

[[transcribe_models]]
name = "fixture-both-in-model"
provider = "fixture-both-provider"
alias = "fixture-both-in"
sample_rate = 44100
language = "it"
target_streaming_delay_ms = 300

[[tts_models]]
name = "fixture-both-out-model"
provider = "fixture-both-provider"
alias = "fixture-both-out"
voice = "fixture-both-voice"
response_format = "mp3"
""",
    },
]

#: Every credential variable the documents and the shipped defaults name. Each
#: is set to the sentinel ``voice.py`` arms before the capture, so a resolution
#: is attributable to its variable without a secret ever being read.
CREDENTIAL_VARIABLES = (
    "MISTRAL_API_KEY",
    "FIXTURE_GATEWAY_TOKEN",
    "FIXTURE_LOCAL_TOKEN",
    "FIXTURE_SPEECH_TOKEN",
    "FIXTURE_BOTH_TOKEN",
)
