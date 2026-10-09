"""Shared disposable installer layout; not a runnable test suite."""
from pathlib import Path
import tempfile
import install


class InstallFixture:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name).resolve()
        self.project = self.base / "project space"
        self.project.mkdir()
        self.prefix = self.base / "runtime"
        self.sources = self.base / "sources"
        self.sources.mkdir()
        for name in ("mnemed", "mneme-mcp", "codex", *install.PROGRAMS):
            path = self.sources / name
            path.write_text("# fixture, never executed\n")
            path.chmod(0o700)

    def prepare(self, **options):
        return install.prepare(self.project, self.prefix, self.sources / "mnemed",
                               self.sources / "mneme-mcp", 18765,
                               program_root=options.pop("program_root", self.sources), **options)

    def async_plan(self, **options):
        return self.prepare(recall_mode="async", reader_model="gpt-6.1-sol",
                            reader_codex=self.sources / "codex", **options)
