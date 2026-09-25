"""Throwaway git repositories for the gate-script tests.

The host's git configuration (signing, hooks, an external diff) must not
change what the scripts parse, so every command runs with no system or global
configuration and a fixed identity.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
from pathlib import Path

GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@example.invalid",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
    "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
}


class TempRepo:
    """A repository in a temporary directory. Call `cleanup` when done."""

    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.path = Path(self._tmp.name)
        self._saved = {k: os.environ.get(k) for k in GIT_ENV}
        os.environ.update(GIT_ENV)
        self.git("init", "-q", "-b", "main")

    def cleanup(self) -> None:
        for k, v in self._saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        self._tmp.cleanup()

    def git(self, *args: str) -> str:
        r = subprocess.run(
            ["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.path,
            capture_output=True,
            text=True,
            check=True,
        )
        return r.stdout.strip()

    def write(self, files: dict[str, str | None]) -> None:
        for name, text in files.items():
            p = self.path / name
            if text is None:
                p.unlink()
                continue
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")

    def commit(self, message: str, files: dict[str, str | None] | None = None) -> str:
        """Write `files` (None deletes one), commit them, and return the SHA."""
        self.write(files or {})
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")
