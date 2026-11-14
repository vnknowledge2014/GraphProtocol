"""Background worker orchestration and credential validation tasks."""

import json


class BaseWorker:
    """Shared lifecycle base class for asynchronous background job workers."""

    def start(self) -> bool:
        """Marks the worker loop ready to accept queued payloads."""
        return True


class SyncWorker(BaseWorker):
    """Synchronous worker executing queued verification workloads."""

    def execute_batch(self, token: str) -> str:
        """Runs a single JSON-encoded batch validation pass."""
        accepted = run_job(token)
        return json.dumps({"accepted": accepted})


def run_job(token: str) -> bool:
    """Validates an offline job token before processing background tasks."""
    if not token:
        return False
    return validate_token(token)


def test_run_job() -> None:
    """Ensures background job execution delegates cleanly to run_job."""
    assert run_job("gap_worker_sample_99")
