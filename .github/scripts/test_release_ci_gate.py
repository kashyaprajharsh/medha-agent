import copy
import unittest

from release_ci_gate import GateError, REQUIRED_JOBS, check_jobs, latest_run, verify

SHA = "a" * 40
REPO = "owner/repo"


def run(**changes):
    value = dict(id=7, run_number=10, run_attempt=1, head_sha=SHA, event="push",
                 head_branch="main", head_repository={"full_name": REPO},
                 path=".github/workflows/ci.yml", status="completed", conclusion="success")
    value.update(changes)
    return value


def jobs():
    return [dict(id=index, run_id=7, run_attempt=1, head_sha=SHA, name=name,
                 status="completed", conclusion="success")
            for index, name in enumerate(sorted(REQUIRED_JOBS))]


class ReleaseGate(unittest.TestCase):
    def test_success_uses_completed_jobs_without_starting_ci(self):
        requested = []

        def fetch(path):
            requested.append(path)
            if "/workflows/" in path:
                return {"workflow_runs": [run()]}
            if "/jobs?" in path:
                return {"total_count": len(jobs()), "jobs": jobs()}
            return run()

        self.assertEqual(verify(REPO, SHA, fetch=fetch)["id"], 7)
        self.assertEqual(len(requested), 4)
        self.assertTrue(all("dispatch" not in path for path in requested))

    def test_missing_ci_wrong_commit_fork_and_pull_request_are_refused(self):
        for value in [[], [run(head_sha="b" * 40)], [run(event="pull_request")],
                      [run(head_branch="topic")], [run(head_repository={"full_name": "fork/repo"})]]:
            with self.subTest(value=value), self.assertRaises(GateError):
                latest_run(value, REPO, SHA)

    def test_newer_failure_cannot_use_old_success(self):
        values = [run(), run(id=8, run_number=11, conclusion="failure")]
        with self.assertRaisesRegex(GateError, "Latest trusted CI"):
            verify(REPO, SHA, fetch=lambda _: {"workflow_runs": values})

    def test_missing_skipped_failed_wrong_run_or_wrong_commit_jobs_are_refused(self):
        scenarios = [jobs()[:-1]]
        for changes in [dict(conclusion="skipped"), dict(conclusion="failure"),
                        dict(head_sha="b" * 40), dict(run_id=8)]:
            values = jobs()
            values[0].update(changes)
            scenarios.append(values)
        for values in scenarios:
            with self.subTest(values=values), self.assertRaises(GateError):
                check_jobs(values, SHA, 7)

    def test_partial_rerun_reuses_successful_jobs_and_latest_attempt_wins(self):
        values = jobs()
        retried = copy.deepcopy(values[0])
        values[0]["conclusion"] = "failure"
        retried.update(id=100, run_attempt=2)
        check_jobs(values + [retried], SHA, 7)
        retried["conclusion"] = "failure"
        with self.assertRaises(GateError):
            check_jobs(jobs() + [retried], SHA, 7)

    def test_in_progress_ci_waits_instead_of_accepting_an_earlier_success(self):
        called = []
        remaining = [run(status="in_progress", conclusion=None), run()]

        def fetch(path):
            if "/workflows/" in path:
                return {"workflow_runs": [remaining.pop(0) if remaining else run()]}
            if "/jobs?" in path:
                return {"total_count": len(jobs()), "jobs": jobs()}
            return run()

        self.assertEqual(verify(REPO, SHA, fetch=fetch, sleep=called.append)["id"], 7)
        self.assertEqual(called, [10])

    def test_rerun_started_during_verification_invalidates_the_completed_attempt(self):
        with self.assertRaisesRegex(GateError, "deadline"):
            verify(REPO, SHA, timeout=0, fetch=lambda path: (
                {"workflow_runs": [run()]} if "/workflows/" in path else
                {"total_count": len(jobs()), "jobs": jobs()} if "/jobs?" in path else
                run(run_attempt=2, status="queued", conclusion=None)))


if __name__ == "__main__":
    unittest.main()
