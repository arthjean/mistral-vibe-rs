from __future__ import annotations

import asyncio
import json
from pathlib import Path
import tempfile

from dotenv import set_key, unset_key
from rich.text import Text
from textual.app import App, ComposeResult
from textual.widget import Widget
from textual.widgets import OptionList

from vibe.app_server.models import (
    MCPSourceKind,
    MCPSourceStatus,
    MCPSourceSummary,
    MCPState,
    MCPToolSummary,
    ScheduledLoop,
    VibeCodePickerContext,
    VibeCodeProject,
    VibeCodeProjectLink,
    VibeCodeRepository,
)
from vibe.app_server.config import ProxySettingsView
from vibe.app_server.protocol import (
    ConfigFieldKind,
    ConfigFieldWire,
    ConfigLayerValueWire,
    MCPAuthUrlParams,
)
from vibe.cli.textual_ui.mcp_commands import parse_mcp_add_args
from vibe.cli.textual_ui.scheduled_loop_runner import _format_loop_list
import vibe.cli.textual_ui.scheduled_loop_runner as loop_runner
from vibe.cli.textual_ui.screens.config._common import (
    filter_field_views,
    format_value,
    origin_label,
    row_text,
)
import vibe.cli.textual_ui.widgets.connector_auth_app as connector_auth_app
import vibe.cli.textual_ui.widgets.mcp_oauth_app as mcp_oauth_app
from vibe.cli.textual_ui.widgets.connector_auth_app import ConnectorAuthApp
from vibe.cli.textual_ui.widgets.mcp_oauth_app import MCPOAuthApp
from vibe.cli.textual_ui.widgets.mcp_app import (
    MCPApp,
    _sort_sources_for_menu,
    _source_option_id,
    _source_status,
    _tool_count_text,
)
from vibe.cli.textual_ui.widgets.no_markup_static import NoMarkupStatic
from vibe.cli.textual_ui.widgets.proxy_setup_app import ProxySetupApp
from vibe.cli.textual_ui.widgets.teleport_message import TeleportMessage
from vibe.cli.textual_ui.widgets.vibe_code_project.picker import (
    VibeCodeProjectPickerApp,
    _project_status_label,
    _repo_count_label,
    build_project_picker_items,
)

# `app.py:2992` mounts the picker with this exact title.
PROJECT_PICKER_TITLE = "Vibe Code project"
from vibe.core.proxy_setup import SUPPORTED_PROXY_VARS, parse_proxy_command


class CapturedOption:
    __slots__ = ("identifier", "disabled", "text")

    def __init__(self, identifier: str | None, disabled: bool, text: str) -> None:
        self.identifier = identifier
        self.disabled = disabled
        self.text = text


class Capture:
    __slots__ = ("title", "options", "messages", "detail")

    def __init__(
        self,
        title: str,
        options: list[CapturedOption],
        messages: list[str],
        detail: str,
    ) -> None:
        self.title = title
        self.options = options
        self.messages = messages
        self.detail = detail


class WidgetHarness(App):
    """Mounts a pinned Python widget headlessly so its own render is the oracle."""

    def __init__(self, widget: Widget) -> None:
        super().__init__()
        self._widget = widget
        self.messages: list[str] = []
        forward = widget.post_message

        def record(message: object) -> bool:
            # Only the pinned widgets' own messages are observable behaviour;
            # Textual's internal traffic is transport noise.
            if type(message).__module__.startswith("vibe."):
                self.messages.append(describe_message(message))
            return forward(message)

        widget.post_message = record  # type: ignore[method-assign]

    def compose(self) -> ComposeResult:
        yield self._widget


def describe_message(message: object) -> str:
    fields = [
        f"{name}={value}"
        for name, value in sorted(vars(message).items())
        if not name.startswith("_") and isinstance(value, (str, bool, int))
    ]
    return f"{type(message).__name__}({','.join(fields)})"


def plain(content: object) -> str:
    return content.plain if isinstance(content, Text) else str(content)


def normalize(text: str) -> str:
    return " ".join(text.split())


def capture(
    widget: Widget,
    title_id: str,
    *,
    detail_id: str | None = None,
    keys: tuple[str, ...] = (),
    select: str | None = None,
) -> Capture:
    async def run() -> Capture:
        app = WidgetHarness(widget)
        async with app.run_test() as pilot:
            await pilot.pause()
            option_list = app.query_one(OptionList)
            if select is not None:
                option_list.highlighted = option_list.get_option_index(select)
                await pilot.press("enter")
            for key in keys:
                await pilot.press(key)
            await pilot.pause()
            return Capture(
                plain(app.query_one(title_id, NoMarkupStatic).content),
                [
                    CapturedOption(
                        option.id, option.disabled, normalize(plain(option.prompt))
                    )
                    for option in option_list.options
                ],
                list(app.messages),
                normalize(
                    plain(app.query_one(detail_id, NoMarkupStatic).content)
                    if detail_id is not None
                    else ""
                ),
            )

    return asyncio.run(run())


_SERVER_STATUS = {
    "healthy": MCPSourceStatus.ENABLED,
    "connected": MCPSourceStatus.CONNECTED,
    "auth_required": MCPSourceStatus.NEEDS_AUTH,
    "setup_required": MCPSourceStatus.NEEDS_SETUP,
    "failed": MCPSourceStatus.UNAVAILABLE,
    "disabled": MCPSourceStatus.DISABLED,
}
_CONNECTOR_STATUS = {
    "connected": MCPSourceStatus.CONNECTED,
    "not_required": MCPSourceStatus.CONNECTED,
    "disconnected": MCPSourceStatus.NEEDS_AUTH,
    "setup_required": MCPSourceStatus.NEEDS_SETUP,
    "failed": MCPSourceStatus.UNAVAILABLE,
}


def mcp_state(event: dict) -> tuple[MCPState, dict[str, str]]:
    """Translate the Rust wire payload into the pinned reference models."""
    sources: list[MCPSourceSummary] = []
    connector_names: dict[str, str] = {}
    for raw in event["mcp"].get("mcp", {}).get("sources", []):
        status = (
            MCPSourceStatus.DISABLED
            if not raw.get("enabled", True)
            else _SERVER_STATUS.get(raw.get("status", ""), MCPSourceStatus.ENABLED)
        )
        sources.append(
            MCPSourceSummary(
                name=raw["name"],
                kind=MCPSourceKind.SERVER,
                transport=raw.get("transport", ""),
                status=status,
                tools=[
                    MCPToolSummary(
                        name=tool["name"],
                        description=tool.get("description", ""),
                        enabled=tool.get("enabled", True),
                    )
                    for tool in raw.get("tools", [])
                ],
            )
        )
    for raw in event["connectors"].get("connectors", {}).get("sources", []):
        name = raw.get("name") or raw["id"]
        connector_names[raw["id"]] = name
        status = (
            MCPSourceStatus.DISABLED
            if not raw.get("enabled", True)
            else _CONNECTOR_STATUS.get(
                raw.get("authState", ""), MCPSourceStatus.UNAVAILABLE
            )
        )
        disabled_tools = set(raw.get("disabledTools", []))
        sources.append(
            MCPSourceSummary(
                name=name,
                kind=MCPSourceKind.CONNECTOR,
                transport="connector",
                status=status,
                tools=[
                    MCPToolSummary(name=tool, enabled=tool not in disabled_tools)
                    for tool in raw.get("toolNames", [])
                ],
            )
        )
    return MCPState(sources=sources), connector_names


def overlay(label: str, title: str, notice: str | None, items: list[tuple[str, bool, str]]) -> str:
    encoded = ";".join(
        f"{name}:{'disabled' if disabled else 'enabled'}:{description}"
        for name, disabled, description in items
    )
    return f"{label}|{title}|{notice or '-'}|{encoded}"


def observe_config(event: dict) -> str:
    snapshot = event["snapshot"]
    schema = event["schema"]
    config = snapshot["config"]
    layers = snapshot.get("layerValues", [])
    items: list[tuple[str, bool, str]] = [
        ("Save changes to", False, "project configuration layer"),
        ("Popular settings", True, ""),
    ]
    popular = {"active_model", "thinking", "theme"}
    flat: list[tuple[str, object, dict | None, str]] = []

    def walk(values: dict, properties: dict, prefix: list[str]) -> None:
        keys = sorted(set(values) | set(properties))
        for key in keys:
            value = values.get(key)
            field = properties.get(key)
            path = [*prefix, key]
            if isinstance(value, dict) or (field and field.get("type") == "object"):
                walk(value or {}, (field or {}).get("properties", {}), path)
            else:
                origin = "effective"
                found_layer = False
                for layer in reversed(layers):
                    cursor = layer.get("values", {})
                    for segment in path:
                        if not isinstance(cursor, dict) or segment not in cursor:
                            break
                        cursor = cursor[segment]
                    else:
                        raw_layer = layer["layer"]
                        if raw_layer == "selected_toml":
                            raw_layer = f"{snapshot.get('selectedTarget', 'selected')}-toml"
                        origin = origin_label(raw_layer)
                        found_layer = True
                        break
                if value is None and not found_layer:
                    origin = origin_label("defaults")
                flat.append((" › ".join(segment.replace("_", " ") for segment in path), value, field, origin))

    walk(config, schema.get("properties", {}), [])
    for group_popular, heading in [(True, None), (False, "Advanced settings")]:
        if heading:
            items.append((heading, True, ""))
        for name, value, field, origin in flat:
            root = name.split(" › ", 1)[0].replace(" ", "_")
            if (root in popular) != group_popular:
                continue
            kind = "schema unavailable"
            disabled = field is None or field.get("writeOnly", False)
            if field:
                raw_kind = field.get("type", "string")
                if isinstance(raw_kind, list):
                    raw_kind = next((item for item in raw_kind if item != "null"), "string")
                kind = {"number": "number", "integer": "integer", "boolean": "boolean"}.get(raw_kind, "string")
            elif isinstance(value, (int, float)) and not isinstance(value, bool):
                kind = "number"
            rendered = format_value(value)
            assert normalize(str(config_row(name, value))) == normalize(
                f"{name} {rendered}"
            )
            description = (field or {}).get("description", "schema unavailable; read-only")
            items.append((name, disabled, f"{rendered} · {kind} · {origin} · {description}"))
    return overlay("config", "Settings", None, items)


def config_row(name: str, value: object) -> object:
    """Render one settings row with the pinned reference renderer."""
    return row_text(
        ConfigFieldWire(
            name=name,
            kind=ConfigFieldKind.STR,
            description="",
            value=value,
            path=f"/{name}",
        )
    )


def observe_proxy(event: dict) -> str:
    settings = event["settings"]
    # The wire payload is an unordered object; the reference server emits the keys
    # in `SUPPORTED_PROXY_VARS` order and the widget renders them in that order.
    view = ProxySettingsView(
        values={key: settings["values"].get(key) for key in SUPPORTED_PROXY_VARS},
        descriptions={
            key: settings["descriptions"][key] for key in SUPPORTED_PROXY_VARS
        },
    )

    async def run() -> tuple[str, list[tuple[str, bool, str]]]:
        app = WidgetHarness(ProxySetupApp(view))
        async with app.run_test() as pilot:
            await pilot.pause()
            title = plain(app.query_one(".settings-title", NoMarkupStatic).content)
            return title, [
                (
                    key,
                    False,
                    f"{field.value or 'not set'} · {field.placeholder}",
                )
                for key, field in app.query_one(ProxySetupApp).inputs.items()
            ]

    title, items = asyncio.run(run())
    assert [key for key, _, _ in items] == list(SUPPORTED_PROXY_VARS), items
    return overlay("proxy", title, None, items)


def observe_proxy_mutation(event: dict) -> str:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / ".env"
        path.write_text(event["initial"])
        # The wire payload is unordered; the reference writes the supported keys
        # in their canonical order.
        changes = {
            key: event["changes"][key]
            for key in SUPPORTED_PROXY_VARS
            if key in event["changes"]
        }
        try:
            for key, value in changes.items():
                if value is not None and ("\n" in value or "\r" in value):
                    raise ValueError("newline")
                if value is None:
                    unset_key(path, key)
                else:
                    set_key(path, key, value)
            status = "ok"
        except ValueError:
            path.write_text(event["initial"])
            status = "error"
        return f"proxy-fs|{status}|{json.dumps(path.read_text(), separators=(',', ':'))}"


def source_line(source: MCPSourceSummary) -> tuple[str, str]:
    """Compose a source row from the pinned helpers, never from a local rule."""
    tools = _tool_count_text(
        sum(tool.enabled for tool in source.tools), len(source.tools)
    )
    symbol, _style, status = _source_status(source)
    rendered = normalize(
        f"{source.name} [{source.transport}] {tools} {symbol} {status}"
    )
    return rendered, f"{source.transport} · {tools} · {status}"


def observe_mcp(event: dict) -> str:
    state, _ = mcp_state(event)
    captured = capture(MCPApp(state), "#mcp-title")
    by_option = {
        _source_option_id(source.name, source.kind): source for source in state.sources
    }
    items: list[tuple[str, bool, str]] = []
    for option in captured.options:
        source = by_option.get(option.identifier or "")
        if source is None:
            items.append((option.text, option.disabled, ""))
            continue
        rendered, description = source_line(source)
        assert option.text == rendered, f"{option.text!r} != {rendered!r}"
        items.append((source.name, option.disabled, description))
    return overlay("mcp", captured.title, None, items)


def tool_line(tool: MCPToolSummary) -> tuple[str, str]:
    description = tool.description
    if not tool.enabled:
        description = f"{description} (disabled)".strip()
    rendered = normalize(
        f"{tool.name}"
        f"{f'  -  {tool.description}' if tool.description else ''}"
        f"{'  (disabled)' if not tool.enabled else ''}"
    )
    return rendered, description


def observe_mcp_detail(event: dict) -> str:
    state, connector_names = mcp_state(event)
    kind = (
        MCPSourceKind.SERVER
        if event["source_kind"] == "server"
        else MCPSourceKind.CONNECTOR
    )
    name = (
        event["source"]
        if kind is MCPSourceKind.SERVER
        else connector_names.get(event["source"], event["source"])
    )
    captured = capture(MCPApp(state, initial_source=name), "#mcp-title")
    source = next(
        item for item in state.sources if item.name == name and item.kind is kind
    )
    tools = {f"tool:{tool.name}": tool for tool in source.tools}
    items: list[tuple[str, bool, str]] = []
    for option in captured.options:
        tool = tools.get(option.identifier or "")
        if tool is None:
            items.append((option.text, option.disabled, ""))
            continue
        rendered, description = tool_line(tool)
        assert option.text == rendered, f"{option.text!r} != {rendered!r}"
        items.append((tool.name, option.disabled, description))
    for message in captured.messages:
        items.append((message, True, ""))
    return overlay("mcp-detail", captured.title, None, items)


class StubLoginClient:
    def __init__(self, url: str) -> None:
        self._url = url
        self.calls: list[str] = []

    async def login(self, name: str):
        self.calls.append(f"mcp/login:{name}")
        yield MCPAuthUrlParams(name=name, url=self._url)
        await asyncio.Event().wait()


class StubConnectorClient:
    def __init__(self, url: str, tool_count: int = 0) -> None:
        self._url = url
        self._tool_count = tool_count
        self.calls: list[str] = []

    async def connector_auth_url(self, name: str) -> str | None:
        self.calls.append(f"connectors/auth-url:{name}")
        return self._url

    async def refresh_connector(self, name: str) -> int:
        self.calls.append(f"connectors/refresh:{name}")
        return self._tool_count


def auth_widget(event: dict) -> tuple[Widget, str, str, list[str]]:
    if event["source_kind"] == "server":
        login = StubLoginClient(event["url"])
        return (
            MCPOAuthApp(event["source"], login),
            "#mcpoauth-title",
            "#mcpoauth-detail",
            login.calls,
        )
    client = StubConnectorClient(event["url"])
    return (
        ConnectorAuthApp(event["source"], client),
        "#connectorauth-title",
        "#connectorauth-detail",
        client.calls,
    )


def observe_auth(event: dict) -> str:
    widget, title_id, detail_id, _ = auth_widget(event)
    captured = capture(widget, title_id, detail_id=detail_id)
    items = [(option.text, option.disabled, "") for option in captured.options]
    return overlay("auth", captured.title, captured.detail, items)


_REFERENCE_ACTION_KEYS = {
    "open": ("open", ()),
    "copy": ("copy", ()),
    "show": ("show", ()),
    "refresh": (None, ("r",)),
    "close": (None, ("escape",)),
}


def observe_auth_action(event: dict) -> str:
    action = event["action"]
    if action not in _REFERENCE_ACTION_KEYS:
        return f"reference|{action}|unsupported"
    select, keys = _REFERENCE_ACTION_KEYS[action]
    widget, title_id, detail_id, client_calls = auth_widget(event)
    module = (
        mcp_oauth_app if event["source_kind"] == "server" else connector_auth_app
    )
    opened: list[str] = []
    copied: list[str] = []
    original_open = module.webbrowser.open
    original_copy = module.copy_text_to_clipboard
    module.webbrowser.open = lambda url, *args, **kwargs: opened.append(url) or True
    module.copy_text_to_clipboard = lambda _app, text, **kwargs: copied.append(text)
    try:
        captured = capture(
            widget, title_id, detail_id=detail_id, select=select, keys=keys
        )
    finally:
        module.webbrowser.open = original_open
        module.copy_text_to_clipboard = original_copy
    calls = (
        [f"url/open:{url}" for url in opened]
        + [f"clipboard/write:{text}" for text in copied]
        + list(client_calls)
        + list(captured.messages)
    )
    return (
        f"reference|{action}|{event['source_kind']}|{event['source']}"
        f"|detail={captured.detail}|calls={','.join(calls)}"
    )


def observe_projects(event: dict) -> str:
    view = event["view"]
    context = view.get("context", {})
    projects = [
        VibeCodeProject(
            project_id=item["projectId"],
            name=item["name"],
            repositories=[VibeCodeRepository(repo_url=repo["repoUrl"]) for repo in item.get("repositories", [])],
            is_read_only=item.get("isReadOnly", False),
        )
        for item in view.get("state", {}).get("projects", [])
    ]
    saved = context.get("savedLink")
    picker_context = VibeCodePickerContext(
        repo_root="/workspace",
        repo_url=context.get("repoUrl", ""),
        repo_name=context.get("repoName", ""),
        saved_link=VibeCodeProjectLink(repo_root="/workspace", repo_url=saved.get("repoUrl", ""), project_id=saved["projectId"], project_name=saved["projectName"]) if saved else None,
    )
    has_more = bool(view.get("state", {}).get("nextCursor"))
    widget = VibeCodeProjectPickerApp(
        context=picker_context,
        projects=projects,
        has_more=has_more,
        include_unlink=bool(saved),
        title=PROJECT_PICKER_TITLE,
    )
    captured = capture(
        widget, ".vibecodeprojectpicker-title", detail_id=".vibecodeprojectpicker-repo"
    )
    built = {
        item.option_id: item
        for item in build_project_picker_items(
            context=picker_context,
            projects=projects,
            has_more=has_more,
            include_unlink=bool(saved),
        )
    }
    items: list[tuple[str, bool, str]] = []
    for option in captured.options:
        item = built.get(option.identifier or "")
        if item is None:
            items.append((option.text, option.disabled, ""))
            continue
        rendered, label, description = project_line(item)
        assert option.text == rendered, f"{option.text!r} != {rendered!r}"
        items.append((label, option.disabled, description))
    return overlay("projects", captured.title, captured.detail, items)


_ACTION_NAMES = {
    "create": "Create new project",
    "load_more": "Load more projects...",
    "unlink": "Unlink project",
}


def project_line(item: object) -> tuple[str, str, str]:
    """Compose a picker row from the pinned item model, never from a local rule."""
    if item.kind == "project":
        repositories = _repo_count_label(item.project)
        status = _project_status_label(item)
        return (
            normalize(f"{item.project.name} {repositories} {status}"),
            item.project.name,
            f"{repositories} · {status}",
        )
    name = _ACTION_NAMES[item.kind]
    return normalize(f"{name} {item.label}"), name, item.label


def observe_event(event: dict) -> str:
    kind = event["kind"]
    if kind == "config": return observe_config(event)
    if kind == "proxy": return observe_proxy(event)
    if kind == "proxy_mutation": return observe_proxy_mutation(event)
    if kind == "mcp": return observe_mcp(event)
    if kind == "mcp_detail": return observe_mcp_detail(event)
    if kind == "mcp_auth": return observe_auth(event)
    if kind == "auth_action": return observe_auth_action(event)
    if kind == "projects": return observe_projects(event)
    if kind == "loops":
        original = loop_runner.time.time
        loop_runner.time.time = lambda: float(event["now_seconds"])
        try:
            return _format_loop_list([ScheduledLoop(id=item["id"], prompt=item["prompt"], interval_seconds=item["intervalSeconds"], next_fire_at=item["nextFireAt"]) for item in event["loops"]])
        finally:
            loop_runner.time.time = original
    event_data = event["event"]
    if event_data["kind"] == "push_required": return f"Streaming:Teleport requires pushing {event_data['unpushedCount']} commits. Use `/teleport approve` or `/teleport deny`."
    if event_data["kind"] == "complete": return f"Completed:Teleported to Vibe Code Web: {event_data['url']}"
    if event_data["kind"] == "failed": return f"Failed:Teleport failed: {event_data['error']['message']}"
    return "Cancelled:Teleport cancelled."


def main() -> None:
    corpus = json.loads(
        Path(__file__).with_name("configuration-integrations-ep009.json").read_text()
    )
    mcp = parse_mcp_add_args(
        "https://mcp.example/rpc --name github --scope repo --scope read"
    )
    context = VibeCodePickerContext(
        repo_root="/workspace",
        repo_url="https://github.com/acme/repo.git",
        repo_name="repo",
        saved_link=VibeCodeProjectLink(
            repo_root="/workspace",
            repo_url="https://github.com/acme/repo.git",
            project_id="current",
            project_name="Current",
        ),
    )
    projects = [
        VibeCodeProject(
            project_id="other",
            name="Other",
            repositories=[
                VibeCodeRepository(repo_url="https://github.com/acme/repo")
            ],
        ),
        VibeCodeProject(
            project_id="current",
            name="Current",
            repositories=[
                VibeCodeRepository(repo_url="git@github.com:acme/repo.git")
            ],
        ),
        VibeCodeProject(
            project_id="readonly",
            name="Read only",
            is_read_only=True,
            repositories=[
                VibeCodeRepository(repo_url="https://github.com/acme/repo")
            ],
        ),
    ]
    items = build_project_picker_items(
        context=context,
        projects=projects,
        has_more=True,
        include_unlink=True,
    )
    fields = [
        ConfigFieldWire(
            name="active_model",
            kind=ConfigFieldKind.ENUM,
            description="Model",
            value="mistral-large",
            path="active_model",
            popular=True,
            enum_choices=["mistral-large", "codestral"],
            layer_values=[
                ConfigLayerValueWire(layer="project-toml", value="mistral-large")
            ],
        ),
        ConfigFieldWire(
            name="notifications",
            kind=ConfigFieldKind.STR,
            description="Notifications",
            value="unfocused",
            path="notifications",
        ),
    ]
    sources = [
        MCPSourceSummary(
            name="github",
            kind=MCPSourceKind.SERVER,
            transport="streamable-http",
            status=MCPSourceStatus.NEEDS_AUTH,
            tools=[],
        ),
        MCPSourceSummary(
            name="drive",
            kind=MCPSourceKind.CONNECTOR,
            transport="streamable-http",
            status=MCPSourceStatus.CONNECTED,
            tools=[MCPToolSummary(name="search", enabled=True)],
        ),
    ]
    original_time = loop_runner.time.time
    loop_runner.time.time = lambda: 100.0
    try:
        loops = _format_loop_list(
            [
                ScheduledLoop(
                    id="loop-1",
                    prompt="check | deploy\nreport",
                    interval_seconds=3661,
                    next_fire_at=130.0,
                )
            ]
        )
    finally:
        loop_runner.time.time = original_time
    teleport = TeleportMessage()
    teleport.set_status("Pushing 2 commits...")
    pushing = teleport.get_content()
    teleport.set_complete("https://chat.mistral.ai/code/project-1")
    complete = teleport.get_content()
    failed = TeleportMessage()
    failed.set_error("cloud unavailable")
    observation = {
        "traceExpected": {
            trace["id"]: [observe_event(event) for event in trace["events"]]
            for trace in corpus["traces"]
        },
        "traceCoverage": sorted(
            {event["kind"] for trace in corpus["traces"] for event in trace["events"]}
        ),
        "config": {
            "filtered": [field.name for field in filter_field_views(fields, "model")],
            "origin": origin_label(fields[0].origin),
            "values": [format_value(field.value) for field in fields],
        },
        "proxy": {
            "keys": list(SUPPORTED_PROXY_VARS),
            "parse": list(parse_proxy_command("https_proxy https://proxy.example")),
        },
        "mcpAdd": {
            "url": mcp.url,
            "name": mcp.name,
            "scopes": mcp.scopes,
            "transport": mcp.transport,
            "login": mcp.login,
        },
        "mcp": [
            {
                "optionId": _source_option_id(source.name, source.kind),
                "status": _source_status(source)[2],
                "tools": _tool_count_text(
                    sum(tool.enabled for tool in source.tools), len(source.tools)
                ),
            }
            for source in _sort_sources_for_menu(sources)
        ],
        "projects": [
            {
                "kind": item.kind,
                "optionId": item.option_id,
                "recommended": item.recommended,
                "name": getattr(getattr(item, "project", None), "name", None),
                "label": item.label,
            }
            for item in items
        ],
        "loops": loops,
        "teleport": {
            "pushing": pushing,
            "complete": complete,
            "failed": failed.get_content(),
        },
    }
    print(json.dumps(observation, separators=(",", ":"), sort_keys=True))


if __name__ == "__main__":
    main()
