"""Deterministic worker/foreground handoffs; not a runnable test suite."""
import queue
import threading
from unittest.mock import patch


class PublicationHandoff:
    """Park a test worker after publication has released the reader-state lock.

    Production hooks intentionally never wait for this lock: a busy optional
    boundary may deliver nothing. Tests of successful delivery/reset must choose
    a free boundary, rather than racing the worker's next idle-state inspection.
    No state transition, reservation or production lock behavior is replaced.
    """

    def __init__(self, reader, session):
        self.reader = reader
        self.session = session
        self.publications = queue.Queue()
        self.gates = []
        self.lock = threading.Lock()
        self.closed = False

    def __enter__(self):
        original = self.reader._state

        def state(config, session, mutate, **kwargs):
            result = original(config, session, mutate, **kwargs)
            if session == self.session and getattr(mutate, "__name__", None) == "publish" and result[0]:
                gate = threading.Event()
                with self.lock:
                    if self.closed:
                        return result
                    self.gates.append(gate)
                    self.publications.put(gate)
                if not gate.wait(10):
                    raise AssertionError("test did not release published worker")
            return result

        self.patch = patch.object(self.reader, "_state", state)
        self.patch.start()
        return self

    def wait(self):
        try:
            return self.publications.get(timeout=5)
        except queue.Empty as error:
            raise AssertionError("worker did not complete a successful publication") from error

    def finish(self):
        """Release every parked publication before joining the test worker."""
        with self.lock:
            self.closed = True
            for gate in self.gates:
                gate.set()

    def __exit__(self, *_):
        self.finish()
        self.patch.stop()
