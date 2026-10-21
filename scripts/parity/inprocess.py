"""What the captures that import the reference into their own process share.

A capture that calls ``vibe`` directly instead of driving one of its console
scripts owns the process the reference runs in, so it guards the network
itself (:class:`SocketGuard`), sets the environment or replaces a reference
attribute around one scenario at a time (:func:`patched_environ`,
:func:`patched`), and records a string the reference authors by its length and
SHA-256 (:func:`digest`). Nothing here imports the reference.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator
import contextlib
import hashlib
import os
import socket
from typing import Any

from reference import OracleError


class SocketGuard:
    """Fails the capture the moment anything tries to reach a network.

    ``connect``, ``connect_ex``, ``create_connection`` and ``getaddrinfo`` are
    replaced with raisers that remember the attempt; ``socketpair`` stays,
    because asyncio's self-pipe uses it without connecting anywhere. Nothing a
    capture drives has a reason to open a socket, and the guard is what turns
    that from a belief into a measurement: a corpus written from a live backend
    is not a corpus of the pinned reference.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []
        self._installed = False

    def refuse(self, name: str) -> Callable[..., Any]:
        """A replacement for ``name`` that records the attempt and raises."""

        def raiser(*arguments: Any, **keywords: Any) -> Any:
            self.attempts.append(name)
            raise OracleError(f"the capture attempted network access through {name}")

        return raiser

    def install(self) -> None:
        if self._installed:
            return
        self._installed = True
        socket.socket.connect = self.refuse("socket.connect")  # type: ignore[method-assign]
        socket.socket.connect_ex = self.refuse("socket.connect_ex")  # type: ignore[method-assign]
        socket.create_connection = self.refuse("socket.create_connection")  # type: ignore[assignment]
        socket.getaddrinfo = self.refuse("socket.getaddrinfo")  # type: ignore[assignment]


@contextlib.contextmanager
def patched_environ(values: dict[str, str | None]) -> Iterator[None]:
    """Sets each of ``values`` (``None`` removes the variable) for the block,
    then restores what the environment held before."""

    originals = {name: os.environ.get(name) for name in values}
    for name, value in values.items():
        if value is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = value
    try:
        yield
    finally:
        for name, original in originals.items():
            if original is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = original


@contextlib.contextmanager
def patched(target: Any, name: str, value: Any) -> Iterator[None]:
    """Replaces the attribute ``name`` of ``target`` with ``value`` for the
    block."""

    original = getattr(target, name)
    setattr(target, name, value)
    try:
        yield
    finally:
        setattr(target, name, original)


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content.

    A digest still fails the replay on any change, while no sentence the
    reference authored ships in a corpus.
    """

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }
