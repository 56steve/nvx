#!/usr/bin/env python3
# pyright: reportPrivateUsage=false

import os
import socket
import struct
import sys
import threading
import time
import unittest
import uuid
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from nvx_tools import control_session  # noqa: E402

if sys.platform == "win32":
    import _winapi


def _read_like_openvmm(descriptor: int) -> bytes | None:
    """Reads a capability pipe as OpenVMM reads --microvm-control-auth-stdin.

    OpenVMM switches the pipe to non-blocking mode and reads it at once, so it
    gets the capability only if the capability and the end of file are already
    there. Returns what the pipe held, or None if a read would block because the
    pipe's write end is still open.
    """
    if sys.platform == "win32":
        import _winapi
        import msvcrt

        handle = msvcrt.get_osfhandle(descriptor)
        pipe_nowait = 1
        _winapi.SetNamedPipeHandleState(handle, pipe_nowait, None, None)

        def read() -> bytes | None:
            try:
                data, _ = _winapi.ReadFile(handle, 64)
            except OSError as error:
                if error.winerror == _winapi.ERROR_BROKEN_PIPE:
                    return b""
                if error.winerror == _winapi.ERROR_NO_DATA:
                    return None
                raise
            return data

    else:
        os.set_blocking(descriptor, False)

        def read() -> bytes | None:
            try:
                return os.read(descriptor, 64)
            except BlockingIOError:
                return None

    data = b""
    while True:
        chunk = read()
        if chunk is None:
            return None
        if not chunk:
            return data
        data += chunk


def _read_exact(connection: socket.socket, length: int) -> bytes:
    output = bytearray()
    while len(output) != length:
        chunk = connection.recv(length - len(output))
        if not chunk:
            raise RuntimeError("test control connection closed")
        output.extend(chunk)
    return bytes(output)


def _read_outer(connection: socket.socket):
    header = _read_exact(connection, control_session.OUTER_HEADER.size)
    values = control_session.OUTER_HEADER.unpack(header)
    payload = _read_exact(connection, values[-1]) if values[-1] else b""
    return (*values[:-1], payload)


def _app_record(
    *,
    instance_id: bytes,
    sequence: int,
    kind: int,
    request_id: int,
    status: int,
    payload: bytes,
) -> bytes:
    app = (
        control_session.APP_HEADER.pack(
            b"NVXC",
            1,
            kind,
            0,
            request_id,
            status,
            len(payload),
        )
        + payload
    )
    return (
        control_session.OUTER_HEADER.pack(
            b"NVXS",
            1,
            control_session.OUTER_DATA,
            0,
            instance_id,
            1,
            sequence,
            len(app),
        )
        + app
    )


def _write_app(
    connection: socket.socket,
    *,
    instance_id: bytes,
    sequence: int,
    kind: int,
    request_id: int,
    status: int,
    payload: bytes,
) -> None:
    connection.sendall(
        _app_record(
            instance_id=instance_id,
            sequence=sequence,
            kind=kind,
            request_id=request_id,
            status=status,
            payload=payload,
        )
    )


class _RecordingStream:
    """A transport that replays `responses` and records every transfer."""

    def __init__(self, responses: bytes = b"", *, failure: str | None = None):
        self._responses = bytearray(responses)
        # The kind of transfer, "write" or "read", that times out.
        self._failure = failure
        self.transfers: list[tuple[str, float]] = []
        self.closes = 0

    def _transfer(self, kind: str, deadline: float) -> None:
        self.transfers.append((kind, deadline))
        if kind == self._failure:
            raise TimeoutError(f"{kind} timed out")

    def write_all(self, data: bytes, deadline: float) -> None:
        self._transfer("write", deadline)

    def read_exact(self, length: int, deadline: float) -> bytes:
        self._transfer("read", deadline)
        data = bytes(self._responses[:length])
        del self._responses[:length]
        return data

    def close(self) -> None:
        self.closes += 1


if sys.platform == "win32":

    def _create_pipe(quota: int = 65536) -> tuple[Path, int]:
        name = rf"\\.\pipe\nvx-control-session-test-{uuid.uuid4().hex}"
        server = _winapi.CreateNamedPipe(
            name,
            _winapi.PIPE_ACCESS_DUPLEX,
            _winapi.PIPE_WAIT,
            1,
            quota,
            quota,
            0,
            _winapi.NULL,
        )
        return Path(name), server

    def _accept_client(server: int) -> None:
        try:
            _winapi.ConnectNamedPipe(server)
        except OSError as error:
            if error.winerror != _winapi.ERROR_PIPE_CONNECTED:
                raise

    def _pipe_read_exact(handle: int, length: int) -> bytes:
        output = bytearray()
        while len(output) != length:
            chunk, _ = _winapi.ReadFile(handle, length - len(output))
            output.extend(chunk)
        return bytes(output)

    def _accept_attach(server: int, capability: bytes, instance_id: bytes) -> None:
        """Authenticates the client of a pipe as OpenVMM's control endpoint does."""
        _accept_client(server)
        header = control_session.OUTER_HEADER.unpack(
            _pipe_read_exact(server, control_session.OUTER_HEADER.size)
        )
        if (
            header[:4] != (b"NVXS", 1, control_session.OUTER_HOST_ATTACH, 0)
            or _pipe_read_exact(server, header[-1]) != capability
        ):
            raise RuntimeError("test endpoint received an invalid attach record")
        _winapi.WriteFile(
            server,
            control_session.OUTER_HEADER.pack(
                b"NVXS", 1, control_session.OUTER_READY, 0, instance_id, 1, 0, 0
            ),
        )


class ControlSessionTests(unittest.TestCase):
    def test_exec_streams_output_and_returns_bounded_status(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("11" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1

        def serve() -> None:
            (
                magic,
                version,
                record_type,
                flags,
                actual_instance,
                epoch,
                sequence,
                frame,
            ) = _read_outer(server)
            self.assertEqual(
                (
                    magic,
                    version,
                    record_type,
                    flags,
                    actual_instance,
                    epoch,
                    sequence,
                ),
                (b"NVXS", 1, control_session.OUTER_DATA, 0, instance, 1, 0),
            )
            (
                app_magic,
                app_version,
                kind,
                app_flags,
                request_id,
                status,
                payload_length,
            ) = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            self.assertEqual(
                (app_magic, app_version, kind, app_flags, status),
                (b"NVXC", 1, control_session.APP_EXEC, 0, 0),
            )
            payload = frame[control_session.APP_HEADER.size :]
            self.assertEqual(payload_length, len(payload))
            timeout_ms, argc, reserved = struct.unpack("<IHH", payload[:8])
            self.assertEqual((timeout_ms, argc, reserved), (5000, 3, 0))

            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_STDOUT,
                request_id=request_id,
                status=0,
                payload=b"hello",
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=1,
                kind=control_session.APP_STDERR,
                request_id=request_id,
                status=0,
                payload=b"warning",
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=2,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=7,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        result = session.exec(
            ("/bin/sh", "-c", "echo hello"),
            timeout_ms=5000,
            response_timeout=5,
        )
        worker.join(timeout=5)
        session.close()

        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.category, "exit")
        self.assertEqual(result.stdout, b"hello")
        self.assertEqual(result.stderr, b"warning")

    def test_exec_rejects_unbounded_or_relative_arguments(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        with self.assertRaisesRegex(ValueError, "absolute"):
            session.exec(("relative",), timeout_ms=0, response_timeout=1)
        with self.assertRaisesRegex(ValueError, "64"):
            session.exec(
                tuple("/bin/true" for _ in range(65)),
                timeout_ms=0,
                response_timeout=1,
            )
        session.close()
        server.close()

    def test_exec_encodes_extended_cwd_and_environment_exactly(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("33" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        captured: dict[str, object] = {}

        def serve() -> None:
            *_, frame = _read_outer(server)
            header = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            request_id = header[4]
            payload = frame[control_session.APP_HEADER.size :]
            (
                timeout_ms,
                argc,
                extension,
                flags,
                envc,
                cwd_len,
            ) = struct.unpack("<IHHHHI", payload[:16])
            offset = 16
            values: list[bytes] = []
            for _ in range(argc):
                length = struct.unpack("<I", payload[offset : offset + 4])[0]
                offset += 4
                values.append(payload[offset : offset + length])
                offset += length
            cwd = payload[offset : offset + cwd_len]
            offset += cwd_len
            environment: list[bytes] = []
            for _ in range(envc):
                length = struct.unpack("<I", payload[offset : offset + 4])[0]
                offset += 4
                environment.append(payload[offset : offset + length])
                offset += length
            captured.update(
                timeout_ms=timeout_ms,
                extension=extension,
                flags=flags,
                values=values,
                cwd=cwd,
                environment=environment,
                offset=offset,
                payload_len=len(payload),
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=0,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        session.exec(
            ("/bin/sh", "-c", "printf exact"),
            timeout_ms=0xFFFFFFFF,
            response_timeout=5,
            cwd="/tmp/space \N{SNOWMAN}",
            environment=("EMPTY=", "SPACED=a b", "EQUALS=a=b", "UTF8=\N{SNOWMAN}"),
        )
        worker.join(timeout=5)
        session.close()

        self.assertEqual(captured["timeout_ms"], 0xFFFFFFFF)
        self.assertEqual(captured["extension"], control_session.APP_EXEC_EXTENDED)
        self.assertEqual(
            captured["flags"],
            control_session.APP_EXEC_CWD_PRESENT
            | control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        self.assertEqual(captured["values"], [b"/bin/sh", b"-c", b"printf exact"])
        self.assertEqual(captured["cwd"], "/tmp/space \N{SNOWMAN}".encode())
        self.assertEqual(
            captured["environment"],
            [
                b"EMPTY=",
                b"SPACED=a b",
                b"EQUALS=a=b",
                "UTF8=\N{SNOWMAN}".encode(),
            ],
        )
        self.assertEqual(captured["offset"], captured["payload_len"])

    def _exec_payload(
        self,
        environment: tuple[str, ...] | None,
        *,
        inherit_default_environment: bool = False,
    ) -> bytes:
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = bytes.fromhex("44" * 16)
        session._epoch = 1
        captured = bytearray()

        def serve() -> None:
            *_, frame = _read_outer(server)
            header = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )
            captured.extend(frame[control_session.APP_HEADER.size :])
            _write_app(
                server,
                instance_id=session._instance_id,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=header[4],
                status=0,
                payload=b"exit",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        session.exec(
            ("/bin/true",),
            timeout_ms=0,
            response_timeout=5,
            environment=environment,
            inherit_default_environment=inherit_default_environment,
        )
        worker.join(timeout=5)
        session.close()
        return bytes(captured)

    def test_exec_distinguishes_omitted_and_empty_environment(self):
        omitted = self._exec_payload(None)
        empty = self._exec_payload(())
        self.assertEqual(struct.unpack("<H", omitted[6:8])[0], 0)
        self.assertEqual(struct.unpack("<H", empty[6:8])[0], 1)
        self.assertEqual(
            struct.unpack("<H", empty[8:10])[0],
            control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        self.assertEqual(struct.unpack("<H", empty[10:12])[0], 0)

    def test_exec_layers_only_a_supplied_environment_over_the_defaults(self):
        layered_flags = (
            control_session.APP_EXEC_ENVIRONMENT_PRESENT
            | control_session.APP_EXEC_INHERIT_DEFAULT_ENV
        )
        layered = self._exec_payload(("A=1",), inherit_default_environment=True)
        self.assertEqual(
            struct.unpack("<HHHI", layered[6:16]),
            (control_session.APP_EXEC_EXTENDED, layered_flags, 1, 0),
        )
        self.assertTrue(layered.endswith(struct.pack("<I", 3) + b"A=1"))
        empty = self._exec_payload((), inherit_default_environment=True)
        self.assertEqual(
            struct.unpack("<HHHI", empty[6:16]),
            (control_session.APP_EXEC_EXTENDED, layered_flags, 0, 0),
        )
        replaced = self._exec_payload(("A=1",))
        self.assertEqual(
            struct.unpack("<H", replaced[8:10])[0],
            control_session.APP_EXEC_ENVIRONMENT_PRESENT,
        )
        # An omitted environment already is the default one, so the request stays the
        # legacy one, which the guest agent accepts without an environment.
        self.assertEqual(
            self._exec_payload(None, inherit_default_environment=True),
            self._exec_payload(None),
        )

    def test_exec_accepts_full_uint32_timeout_range_without_waiting(self):
        for timeout in (0, 3_600_001, 86_400_000, 0xFFFFFFFF):
            with self.subTest(timeout=timeout):
                client, server = socket.socketpair()
                instance = bytes.fromhex("55" * 16)
                session = control_session.ControlSession(
                    control_session._SocketStream(client)
                )
                session._instance_id = instance
                session._epoch = 1

                def serve(
                    server_socket: socket.socket = server,
                    expected_timeout: int = timeout,
                    expected_instance: bytes = instance,
                ) -> None:
                    *_, frame = _read_outer(server_socket)
                    header = control_session.APP_HEADER.unpack(
                        frame[: control_session.APP_HEADER.size]
                    )
                    payload = frame[control_session.APP_HEADER.size :]
                    self.assertEqual(
                        struct.unpack("<I", payload[:4])[0],
                        expected_timeout,
                    )
                    _write_app(
                        server_socket,
                        instance_id=expected_instance,
                        sequence=0,
                        kind=control_session.APP_EXIT,
                        request_id=header[4],
                        status=0,
                        payload=b"exit",
                    )
                    server_socket.close()

                worker = threading.Thread(target=serve)
                worker.start()
                session.exec(
                    ("/bin/true",),
                    timeout_ms=timeout,
                    response_timeout=5,
                )
                worker.join(timeout=5)
                session.close()

    def test_exec_rejects_invalid_timeout_and_execution_fields_before_sending(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))
        invalid = (-1, 0x100000000, True, 1.5, "1")
        for timeout in invalid:
            with (
                self.subTest(timeout=timeout),
                self.assertRaises((TypeError, ValueError)),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=timeout,  # type: ignore[arg-type]
                    response_timeout=1,
                )
        for kwargs in (
            {"cwd": ""},
            {"cwd": "relative"},
            {"cwd": "/bad\0path"},
            {"environment": ("MISSING_EQUALS",)},
            {"environment": ("=empty-name",)},
            {"environment": ("DUP=1", "DUP=2")},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=1,
                    **kwargs,  # type: ignore[arg-type]
                )
        for inherit in (1, "true", None):
            with (
                self.subTest(inherit=inherit),
                self.assertRaisesRegex(TypeError, "inheritance must be a boolean"),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=1,
                    environment=("A=1",),
                    inherit_default_environment=inherit,  # type: ignore[arg-type]
                )
        server.setblocking(False)
        with self.assertRaises(BlockingIOError):
            server.recv(1)
        session.close()
        server.close()

    def test_exec_rejects_invalid_response_timeout_before_sending(self):
        client, server = socket.socketpair()
        session = control_session.ControlSession(control_session._SocketStream(client))

        for timeout in (0.0, -1.0, float("nan"), float("inf")):
            with (
                self.subTest(timeout=timeout),
                self.assertRaisesRegex(ValueError, "response timeout"),
            ):
                session.exec(
                    ("/bin/true",),
                    timeout_ms=0,
                    response_timeout=timeout,
                )

        server.setblocking(False)
        with self.assertRaises(BlockingIOError):
            server.recv(1)
        session.close()
        server.close()

    def test_exec_rejects_unknown_exit_category(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("22" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1

        def serve() -> None:
            *_, frame = _read_outer(server)
            request_id = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )[4]
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_EXIT,
                request_id=request_id,
                status=125,
                payload=b"guest-provided-detail",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        with self.assertRaisesRegex(
            control_session.ScriptError, "unsupported exit category"
        ):
            session.exec(("/bin/true",), timeout_ms=0, response_timeout=5)
        worker.join(timeout=5)
        session.close()

    def test_exec_refusal_keeps_status_category_and_diagnostic(self):
        client, server = socket.socketpair()
        instance = bytes.fromhex("33" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        diagnostic = (
            b"nvx-managed-agent: cannot enter working directory /missing: "
            b"No such file or directory\n"
        )

        def serve() -> None:
            *_, frame = _read_outer(server)
            request_id = control_session.APP_HEADER.unpack(
                frame[: control_session.APP_HEADER.size]
            )[4]
            _write_app(
                server,
                instance_id=instance,
                sequence=0,
                kind=control_session.APP_STDERR,
                request_id=request_id,
                status=0,
                payload=diagnostic,
            )
            _write_app(
                server,
                instance_id=instance,
                sequence=1,
                kind=control_session.APP_ERROR,
                request_id=request_id,
                status=2,
                payload=b"cwd-failed",
            )
            server.close()

        worker = threading.Thread(target=serve)
        worker.start()
        with self.assertRaises(control_session.ManagedExecRefused) as raised:
            session.exec(
                ("/bin/true",), timeout_ms=0, response_timeout=5, cwd="/missing"
            )
        worker.join(timeout=5)
        session.close()

        refusal = raised.exception
        self.assertIsInstance(refusal, control_session.ScriptError)
        self.assertEqual(
            (refusal.status, refusal.category, refusal.stdout, refusal.stderr),
            (2, "cwd-failed", b"", diagnostic),
        )
        self.assertEqual(
            str(refusal),
            "managed guest rejected exec (status=2, category=cwd-failed)",
        )

    def test_session_reset_is_not_a_closed_endpoint(self):
        # A reset comes from an OpenVMM that still serves the endpoint, so it must
        # not look like the closure with which OpenVMM shuts down.
        client, server = socket.socketpair()
        instance = bytes.fromhex("11" * 16)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = instance
        session._epoch = 1
        server.sendall(
            control_session.OUTER_HEADER.pack(
                b"NVXS", 1, control_session.OUTER_RESET, 0, instance, 1, 0, 0
            )
        )
        try:
            with self.assertRaisesRegex(
                ConnectionError, "^managed control session was reset$"
            ) as raised:
                session.ping(5)
        finally:
            session.close()
            server.close()

        self.assertNotIsInstance(
            raised.exception, control_session.ControlEndpointClosed
        )

    def test_operations_bound_their_requests_by_their_deadlines(self):
        # Issue #464: an operation's timeout bounds sending its request as well as
        # receiving its response, so attach, ping, exec, and stop each write with
        # the deadline with which they then read.
        instance = bytes.fromhex("66" * 16)
        request_id = 0x1234

        def response(sequence: int, kind: int, payload: bytes = b"") -> bytes:
            return _app_record(
                instance_id=instance,
                sequence=sequence,
                kind=kind,
                request_id=request_id,
                status=0,
                payload=payload,
            )

        stream = _RecordingStream(
            control_session.OUTER_HEADER.pack(
                b"NVXS", 1, control_session.OUTER_READY, 0, instance, 1, 0, 0
            )
            + response(1, control_session.APP_READY)
            + response(2, control_session.APP_EXIT, b"exit")
            + response(3, control_session.APP_STOPPED)
        )
        transport = (
            control_session._NamedPipeStream
            if os.name == "nt"
            else control_session._SocketStream
        )
        with (
            patch.object(transport, "connect", return_value=stream),
            patch.object(
                control_session, "time", SimpleNamespace(monotonic=lambda: 1000.0)
            ),
            patch.object(
                control_session.ControlSession, "_request_id", return_value=request_id
            ),
        ):
            session = control_session.ControlSession.connect(
                Path("endpoint"), bytes(range(1, 33)), 7
            )
            session.ping(5)
            session.exec(("/bin/true",), timeout_ms=0, response_timeout=3)
            session.stop(2)
            session.close()

        self.assertEqual(
            stream.transfers,
            [
                ("write", 1007.0),
                ("read", 1007.0),
                *(
                    transfer
                    for deadline in (1005.0, 1003.0, 1002.0)
                    for transfer in (
                        ("write", deadline),
                        ("read", deadline),
                        ("read", deadline),
                    )
                ),
            ],
        )

    def test_failed_transfer_closes_the_session(self):
        # A transfer that fails, as by timing out, may cut its record short, after
        # which the endpoint would frame every later record wrongly.
        for failure in ("write", "read"):
            with self.subTest(failure=failure):
                stream = _RecordingStream(failure=failure)
                session = control_session.ControlSession(cast(Any, stream))
                with self.assertRaisesRegex(TimeoutError, f"^{failure} timed out$"):
                    session.ping(5)
                self.assertEqual(stream.closes, 1)
                transfers = len(stream.transfers)
                with self.assertRaisesRegex(
                    ValueError, "^managed control session is closed$"
                ):
                    session.exec(("/bin/true",), timeout_ms=0, response_timeout=5)
                session.close()
                self.assertEqual(len(stream.transfers), transfers)
                self.assertEqual(stream.closes, 1)


class CapabilityPipeTests(unittest.TestCase):
    CAPABILITY = bytes(range(1, 33))

    def test_pipe_holds_the_capability_and_then_end_of_file(self):
        descriptor = control_session.capability_pipe(self.CAPABILITY)
        try:
            self.assertEqual(_read_like_openvmm(descriptor), self.CAPABILITY)
        finally:
            os.close(descriptor)

    def test_openvmm_refuses_a_pipe_whose_writer_is_open(self):
        # A launcher that writes the capability only after it starts OpenVMM
        # leaves the writer open when OpenVMM reads, as before issue #440.
        read, write = os.pipe()
        try:
            os.write(write, self.CAPABILITY)
            self.assertIsNone(_read_like_openvmm(read))
        finally:
            os.close(read)
            os.close(write)

    def test_invalid_capabilities_are_rejected(self):
        for capability in (b"", bytes(range(1, 32)), bytes(32), bytes(range(1, 34))):
            with (
                self.subTest(capability=capability.hex()),
                self.assertRaisesRegex(ValueError, "32 nonzero bytes"),
            ):
                control_session.capability_pipe(capability)


class SocketStreamTests(unittest.TestCase):
    def test_write_that_sends_nothing_reports_a_closed_endpoint(self):
        connection = MagicMock(spec=socket.socket)
        connection.send.return_value = 0
        stream = control_session._SocketStream(connection)
        with self.assertRaisesRegex(
            control_session.ControlEndpointClosed,
            "^managed control endpoint closed$",
        ):
            stream.write_all(b"record", time.monotonic() + 60)
        connection.send.assert_called_once()

    def test_read_reports_a_closed_endpoint(self):
        client, server = socket.socketpair()
        stream = control_session._SocketStream(client)
        server.close()
        try:
            with self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ):
                stream.read_exact(1, time.monotonic() + 5)
        finally:
            stream.close()

    def test_reset_and_broken_pipe_report_a_closed_endpoint(self):
        # OpenVMM may close the socket before it reads everything that the client
        # sent, which resets the client's reads, and a later write fails with a
        # broken pipe.
        if sys.platform != "linux":
            self.skipTest("Unix-domain socket closure semantics require Linux")
        client, server = socket.socketpair()
        stream = control_session._SocketStream(client)
        stream.write_all(b"unread request", time.monotonic() + 5)
        server.close()
        try:
            with self.assertRaises(control_session.ControlEndpointClosed) as raised:
                stream.read_exact(1, time.monotonic() + 5)
            self.assertIsInstance(raised.exception.__cause__, ConnectionResetError)
            with self.assertRaises(control_session.ControlEndpointClosed) as raised:
                stream.write_all(b"record", time.monotonic() + 5)
            self.assertIsInstance(raised.exception.__cause__, BrokenPipeError)
        finally:
            stream.close()

    def test_request_to_a_stalled_endpoint_times_out(self):
        # Issue #464: a request that the endpoint does not read must not hold the
        # operation past its timeout.
        if sys.platform != "linux":
            self.skipTest("Unix-domain socket buffering semantics require Linux")
        client, server = socket.socketpair()
        # The request is several times larger than the socket buffer.
        client.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        server.settimeout(5)
        session = control_session.ControlSession(control_session._SocketStream(client))
        session._instance_id = bytes.fromhex("77" * 16)
        session._epoch = 1
        arguments = ("/bin/true", *("a" * 4000,) * 15)
        released = threading.Event()
        received = bytearray()
        failures: list[BaseException] = []

        def serve() -> None:
            try:
                # The peer reads nothing until the client gives up, or long
                # after the client's timeout.
                released.wait(10)
                while chunk := server.recv(65536):
                    received.extend(chunk)
            except BaseException as error:
                failures.append(error)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        timeout = 0.25
        try:
            started = time.monotonic()
            with self.assertRaisesRegex(
                TimeoutError, "^managed control request timed out$"
            ):
                session.exec(arguments, timeout_ms=0, response_timeout=timeout)
            elapsed = time.monotonic() - started
            with self.assertRaisesRegex(
                ValueError, "^managed control session is closed$"
            ):
                session.ping(5)
        finally:
            session.close()
            released.set()
            worker.join(10)
            server.close()

        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])
        self.assertGreaterEqual(elapsed, timeout)
        self.assertLess(elapsed, timeout + 2)
        # The endpoint received only part of the request before the end of stream.
        self.assertLess(len(received), sum(map(len, arguments)))


class NamedPipeStreamTests(unittest.TestCase):
    def test_write_that_moves_nothing_reports_a_closed_endpoint(self):
        # The stream is never closed, since its handle is not real.
        stream = control_session._NamedPipeStream(123)
        with (
            patch.object(control_session, "_winapi") as winapi,
            patch.object(control_session, "_finish_transfer", return_value=0) as finish,
            self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ),
        ):
            winapi.WriteFile.return_value = (MagicMock(), 0)
            stream.write_all(b"record", time.monotonic() + 60)
        finish.assert_called_once()

    def test_write_reports_a_closed_endpoint(self):
        # OpenVMM closes the pipe as it tears the VM down, and the CRT reports a
        # later write to it as EINVAL rather than as a broken pipe.
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        import _winapi

        name = rf"\\.\pipe\nvx-control-session-test-{uuid.uuid4().hex}"
        server = _winapi.CreateNamedPipe(
            name,
            _winapi.PIPE_ACCESS_DUPLEX,
            _winapi.PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            _winapi.NULL,
        )
        try:
            stream = control_session._NamedPipeStream.connect(Path(name), 5)
        finally:
            _winapi.CloseHandle(server)
        try:
            with self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ):
                stream.write_all(b"record", time.monotonic() + 5)
        finally:
            stream.close()

    def test_read_reports_a_closed_endpoint_after_its_data(self):
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        path, server = _create_pipe()
        try:
            stream = control_session._NamedPipeStream.connect(path, 5)
            _accept_client(server)
            _winapi.WriteFile(server, b"ok")
        finally:
            _winapi.CloseHandle(server)
        try:
            self.assertEqual(stream.read_exact(2, time.monotonic() + 5), b"ok")
            with self.assertRaisesRegex(
                control_session.ControlEndpointClosed,
                "^managed control endpoint closed$",
            ):
                stream.read_exact(1, time.monotonic() + 5)
        finally:
            stream.close()

    def test_read_times_out_without_losing_later_data(self):
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        path, server = _create_pipe()
        self.addCleanup(_winapi.CloseHandle, server)
        stream = control_session._NamedPipeStream.connect(path, 5)
        self.addCleanup(stream.close)
        _accept_client(server)

        started = time.monotonic()
        with self.assertRaisesRegex(
            TimeoutError, "^managed control response timed out$"
        ):
            stream.read_exact(2, started + 0.1)
        self.assertLess(time.monotonic() - started, 2.1)
        _winapi.WriteFile(server, b"ok")
        self.assertEqual(stream.read_exact(2, time.monotonic() + 5), b"ok")

    def test_interrupted_write_is_cancelled_before_the_interruption_propagates(self):
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        path, server = _create_pipe(1024)
        self.addCleanup(_winapi.CloseHandle, server)
        stream = control_session._NamedPipeStream.connect(path, 5)
        self.addCleanup(stream.close)
        _accept_client(server)

        with (
            patch.object(
                control_session, "_signaled_before", side_effect=KeyboardInterrupt
            ),
            self.assertRaises(KeyboardInterrupt),
        ):
            stream.write_all(b"x" * 4096, time.monotonic() + 5)
        # The write, which the pipe could not hold, no longer waits in it.
        self.assertEqual(_winapi.PeekNamedPipe(server)[0], 0)

    def test_request_to_a_stalled_endpoint_times_out_and_closes_the_session(self):
        # Issue #464: an endpoint that authenticated the session and then stopped
        # reading held the request's write, and so the operation, past its timeout.
        if sys.platform != "win32":
            self.skipTest("named pipes require a Windows host")
        # The 4,093-byte request does not fit in the endpoint's 1 KiB quota.
        path, server = _create_pipe(1024)
        capability = bytes(range(1, 33))
        released = threading.Event()
        received = bytearray()
        failures: list[BaseException] = []

        def serve() -> None:
            # Type checkers check a nested function apart from its test's skip.
            assert sys.platform == "win32"
            try:
                _accept_attach(server, capability, bytes.fromhex("11" * 16))
                # The endpoint keeps the pipe open but reads nothing more until
                # the client gives up, or long after the client's timeout.
                released.wait(10)
                while True:
                    chunk, _ = _winapi.ReadFile(server, 65536)
                    received.extend(chunk)
            except BrokenPipeError:
                pass
            except BaseException as error:
                failures.append(error)

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        timeout = 0.25
        try:
            with control_session.ControlSession.connect(path, capability, 5) as session:
                started = time.monotonic()
                with self.assertRaisesRegex(
                    TimeoutError, "^managed control request timed out$"
                ):
                    session.exec(
                        ("/bin/true", "a" * 4000),
                        timeout_ms=0,
                        response_timeout=timeout,
                    )
                elapsed = time.monotonic() - started
                with self.assertRaisesRegex(
                    ValueError, "^managed control session is closed$"
                ):
                    session.ping(5)
        finally:
            released.set()
            worker.join(5)
            _winapi.CloseHandle(server)

        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])
        self.assertGreaterEqual(elapsed, timeout)
        self.assertLess(elapsed, timeout + 2)
        # The endpoint received less than the request before the pipe closed.
        self.assertLess(len(received), 4000)


if __name__ == "__main__":
    unittest.main()
