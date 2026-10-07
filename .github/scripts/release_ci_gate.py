"""Reuse successful CI for the exact release commit; never launch another matrix.

The newest trusted push run and the latest attempt of every required job must
pass. Partial reruns may reuse successful jobs, but older success cannot hide
new failure. API errors, absent jobs and absent CI fail closed.
"""

import argparse
import json
import re
import subprocess
import time


REQUIRED_JOBS = frozenset({
    "lint", "minimum-rust",
    "test (ubuntu-latest)", "test (macos-latest)", "test (windows-latest)",
    "desktop (ubuntu-latest)", "desktop (macos-latest)", "desktop (windows-latest)",
})


class GateError(RuntimeError):
    pass


def api(path):
    try:
        result = subprocess.run(
            ["gh", "api", path], check=True, capture_output=True, text=True, timeout=30,
        )
        return json.loads(result.stdout)
    except (subprocess.SubprocessError, json.JSONDecodeError) as error:
        raise GateError("Cannot verify CI through the GitHub API; publishing is refused") from error


def latest_run(runs, repository, sha):
    trusted = [run for run in runs if (
        run.get("head_sha") == sha
        and run.get("event") == "push"
        and run.get("head_branch") in {"main", "master"}
        and run.get("path") == ".github/workflows/ci.yml"
        and run.get("head_repository", {}).get("full_name", "").casefold() == repository.casefold()
    )]
    if not trusted:
        raise GateError(f"No trusted main/master CI push run exists for {sha}; publishing is refused")
    return max(trusted, key=lambda run: (run["run_number"], run["run_attempt"]))


def check_jobs(jobs, sha, run_id):
    latest = {}
    for job in jobs:
        if job.get("head_sha") != sha or job.get("run_id") != run_id:
            raise GateError("CI jobs do not belong to the exact release commit and run")
        name = job["name"]
        prior = latest.get(name)
        if prior is None or (job["run_attempt"], job["id"]) > (prior["run_attempt"], prior["id"]):
            latest[name] = job
    missing = REQUIRED_JOBS - latest.keys()
    if missing:
        raise GateError("Required CI jobs are missing: " + ", ".join(sorted(missing)))
    failed = [name for name, job in latest.items() if (
        job.get("status") != "completed" or job.get("conclusion") != "success"
    )]
    if failed:
        raise GateError("CI jobs did not pass: " + ", ".join(sorted(failed)))


def verify(repository, sha, timeout=2700, fetch=api, clock=time.monotonic, sleep=time.sleep):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise GateError("Invalid repository identity")
    if not re.fullmatch(r"[a-f0-9]{40}", sha):
        raise GateError("Release CI must be checked against a full immutable commit SHA")
    deadline = clock() + timeout
    while True:
        runs = fetch(f"repos/{repository}/actions/workflows/ci.yml/runs?head_sha={sha}&event=push&per_page=100")
        run = latest_run(runs["workflow_runs"], repository, sha)
        if run.get("status") == "completed":
            if run.get("conclusion") != "success":
                raise GateError(f"Latest trusted CI run {run['id']} concluded {run.get('conclusion')}; publishing is refused")
            jobs = []
            for page in range(1, 21):
                batch = fetch(f"repos/{repository}/actions/runs/{run['id']}/jobs?filter=all&per_page=100&page={page}")
                jobs.extend(batch["jobs"])
                if len(jobs) >= batch["total_count"]:
                    break
            else:
                raise GateError("CI job history exceeded the verification budget")
            check_jobs(jobs, sha, run["id"])
            # A rerun can start while the jobs API is read. Confirm the same
            # completed attempt still owns the result before admitting release.
            checked = fetch(f"repos/{repository}/actions/runs/{run['id']}")
            if checked.get("head_sha") == sha and checked.get("run_attempt") == run["run_attempt"] and checked.get("status") == "completed" and checked.get("conclusion") == "success":
                newest = latest_run(fetch(f"repos/{repository}/actions/workflows/ci.yml/runs?head_sha={sha}&event=push&per_page=100")["workflow_runs"], repository, sha)
                if (newest["id"], newest["run_attempt"]) == (run["id"], run["run_attempt"]) and newest.get("status") == "completed" and newest.get("conclusion") == "success":
                    return run
        if clock() >= deadline:
            raise GateError("CI did not finish before the release verification deadline")
        sleep(min(10, max(0, deadline - clock())))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--sha", required=True)
    args = parser.parse_args()
    try:
        run = verify(args.repository, args.sha)
    except GateError as error:
        parser.exit(1, f"{error}\n")
    print(f"Verified all required CI jobs at {run['html_url']} for {args.sha}")


if __name__ == "__main__":
    main()
