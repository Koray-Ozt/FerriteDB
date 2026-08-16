"""Sidecar process management and Unix socket lifecycle."""

from __future__ import annotations

import os
import platform
import shutil
import signal
import stat
import subprocess
import tempfile
import time
import uuid
from typing import NamedTuple

from .exceptions import FerriteSidecarError, FerriteTimeoutError


class SocketIdentity(NamedTuple):
    """File system identity (device and inode) of a Unix socket."""

    dev: int
    ino: int


def get_socket_identity(path: str) -> SocketIdentity:
    """Retrieves device and inode identifiers for a verified socket path."""
    try:
        st = os.lstat(path)
    except OSError as err:
        raise FerriteSidecarError(f"Cannot stat socket path: {path}") from err

    if not stat.S_ISSOCK(st.st_mode):
        raise FerriteSidecarError(f"FerriteDB socket path is not a socket: {path}")
    return SocketIdentity(dev=st.st_dev, ino=st.st_ino)


def remove_socket_safe(path: str, identity: SocketIdentity | None) -> None:
    """Removes a socket only if it matches the expected identity and is still a socket."""
    if not path or not identity:
        return
    try:
        st = os.lstat(path)
        if stat.S_ISSOCK(st.st_mode) and st.st_dev == identity.dev and st.st_ino == identity.ino:
            os.unlink(path)
    except FileNotFoundError:
        pass
    except OSError:
        pass


def default_binary() -> str:
    """Resolves default FerriteDB sidecar executable."""
    env_bin = os.environ.get("FERRITE_BIN")
    if env_bin and os.path.isfile(env_bin):
        return os.path.abspath(env_bin)

    which_bin = shutil.which("ferrite")
    if which_bin:
        return which_bin

    # Check relative paths when running inside repository
    cur_dir = os.path.abspath(os.path.dirname(__file__))
    for _ in range(6):
        for sub in ("target/debug/ferrite", "target/release/ferrite"):
            cand = os.path.join(cur_dir, sub)
            if os.path.isfile(cand) and os.access(cand, os.X_OK):
                return cand
        parent = os.path.dirname(cur_dir)
        if parent == cur_dir:
            break
        cur_dir = parent

    for sub in ("target/debug/ferrite", "target/release/ferrite"):
        cand = os.path.abspath(os.path.join(os.getcwd(), sub))
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand

    arch = platform.machine().lower()
    sys_name = platform.system().lower()
    raise FerriteSidecarError(
        f"FerriteDB sidecar binary not found on {sys_name}-{arch}. "
        "Set FERRITE_BIN environment variable or provide binary path in OpenOptions."
    )


class SidecarProcess:
    """Manages the lifecycle of a FerriteDB server sidecar process."""

    def __init__(
        self,
        process: subprocess.Popen[bytes],
        socket_path: str,
        identity: SocketIdentity,
    ) -> None:
        self._process = process
        self.socket_path = socket_path
        self.identity = identity
        self._stopped = False

    @property
    def pid(self) -> int | None:
        return self._process.pid

    @classmethod
    def launch(
        cls,
        db_path: str,
        binary: str | None = None,
        socket_path: str | None = None,
        schema: str | None = None,
        timeout: float = 5.0,
    ) -> SidecarProcess:
        """Launches the sidecar binary and waits for the Unix socket to be ready."""
        sock = socket_path or os.path.join(
            tempfile.gettempdir(),
            f"ferrite-{os.getpid()}-{uuid.uuid4().hex[:12]}.sock",
        )
        if os.path.exists(sock):
            raise FerriteSidecarError(f"FerriteDB socket path already exists: {sock}")

        bin_path = binary or default_binary()
        if not os.path.isfile(bin_path):
            raise FerriteSidecarError(f"FerriteDB binary does not exist: {bin_path}")

        args = [bin_path, "serve", db_path, "--socket", sock]
        if schema:
            args.extend(["--schema", schema])

        try:
            process = subprocess.Popen(
                args,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
        except OSError as err:
            raise FerriteSidecarError(f"Failed to execute FerriteDB sidecar '{bin_path}': {err}") from err

        deadline = time.monotonic() + max(timeout, 0.5)
        identity: SocketIdentity | None = None

        try:
            while not os.path.exists(sock):
                exit_code = process.poll()
                if exit_code is not None:
                    stderr = ""
                    if process.stderr:
                        try:
                            stderr = process.stderr.read().decode("utf-8", errors="replace")
                        except Exception:
                            pass
                    raise FerriteSidecarError(
                        f"FerriteDB failed to start (exit code {exit_code}): {stderr.strip()}"
                    )
                if time.monotonic() >= deadline:
                    raise FerriteTimeoutError("FerriteDB sidecar startup timed out")
                time.sleep(0.01)

            identity = get_socket_identity(sock)
            return cls(process, sock, identity)
        except Exception:
            # Clean up on failure
            if process.poll() is None:
                try:
                    process.terminate()
                    process.wait(timeout=0.5)
                except Exception:
                    try:
                        process.kill()
                    except Exception:
                        pass
            if process.stderr:
                try:
                    process.stderr.close()
                except Exception:
                    pass
            if identity:
                remove_socket_safe(sock, identity)
            raise

    def stop(self) -> None:
        """Gracefully terminates the sidecar process and cleans up its socket."""
        if self._stopped:
            return
        self._stopped = True

        if self._process.poll() is None:
            try:
                self._process.terminate()
                self._process.wait(timeout=1.0)
            except (subprocess.TimeoutExpired, OSError):
                try:
                    self._process.kill()
                    self._process.wait(timeout=1.0)
                except OSError:
                    pass

        if self._process.stderr:
            try:
                self._process.stderr.close()
            except Exception:
                pass

        remove_socket_safe(self.socket_path, self.identity)
