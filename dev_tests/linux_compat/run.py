#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

"""Run a pinned, unprivileged gVisor syscall subset on Linux and LiteBox."""

import argparse
from collections import Counter
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time


WORKSPACE = Path(__file__).resolve().parents[2]
MANIFEST = Path(__file__).with_name("gvisor.json")
STATUSES = ("pass", "failure", "crash", "timeout", "skip", "infrastructure_error")


class HarnessError(Exception):
    """A preparation or harness failure, rather than a compatibility result."""


@dataclass
class ProcessResult:
    command: list[str]
    returncode: int | None
    timed_out: bool
    elapsed_seconds: float
    stdout: str
    stderr: str
    error: str | None = None


def run_process(command, directory, *, cwd, env=None, timeout=30):
    """Log a command and kill its private process group, including leftover children."""
    directory.mkdir(parents=True, exist_ok=True)
    stdout_path = directory / "stdout.log"
    stderr_path = directory / "stderr.log"
    started = time.monotonic()
    returncode = None
    timed_out = False
    error = None
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        try:
            process = subprocess.Popen(
                command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                stdout=stdout, stderr=stderr, start_new_session=True, umask=0o022,
            )
        except OSError as failure:
            error = str(failure)
            stderr.write((error + "\n").encode())
        else:
            try:
                # Leave the leader unreaped until cleanup so its process-group ID
                # cannot be recycled underneath us.
                while os.waitid(
                    os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT,
                ) is None:
                    remaining = timeout - (time.monotonic() - started)
                    if remaining <= 0:
                        timed_out = True
                        break
                    time.sleep(min(0.02, remaining))
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                returncode = process.wait()
    result = ProcessResult(
        list(command), returncode, timed_out, time.monotonic() - started,
        str(stdout_path), str(stderr_path), error,
    )
    (directory / "process.json").write_text(json.dumps(asdict(result), indent=2) + "\n")
    return result


def read_log(path):
    return Path(path).read_text(errors="replace")


def checked(command, directory, *, cwd, env=None, timeout=600):
    result = run_process(command, directory, cwd=cwd, env=env, timeout=timeout)
    if result.error or result.timed_out or result.returncode != 0:
        raise HarnessError(
            f"{command[0]} failed (exit={result.returncode}, "
            f"timeout={result.timed_out}, error={result.error}); see {directory}"
        )
    return read_log(result.stdout).strip()


def parse_test_list(output):
    """Parse GoogleTest's listing, including parameterized suite/case names."""
    cases = []
    suite = None
    for line in output.splitlines():
        name = line.split("#", 1)[0].strip()
        if not name:
            continue
        if not line[0].isspace() and name.endswith("."):
            suite = name
        elif line[0].isspace() and suite:
            if not re.fullmatch(r"[\w/]+", name):
                raise HarnessError(f"Unrecognized GoogleTest case: {line!r}")
            cases.append(suite + name)
    if not cases or len(cases) != len(set(cases)):
        raise HarnessError("GoogleTest listing is empty or contains duplicate cases")
    return cases


def validate_selection(test, discovered):
    selected = test["cases"]
    excluded = test["excluded"]
    if not selected or len(selected) != len(set(selected)):
        raise HarnessError(f"{test['binary']}: empty or duplicate selection")
    if set(selected) & set(excluded) or any(not reason for reason in excluded.values()):
        raise HarnessError(f"{test['binary']}: overlapping or unexplained exclusions")
    missing = (set(selected) | set(excluded)) - set(discovered)
    unaccounted = set(discovered) - (set(selected) | set(excluded))
    if missing or unaccounted:
        raise HarnessError(
            f"{test['binary']}: manifest/list mismatch: "
            f"missing={sorted(missing)}, unaccounted={sorted(unaccounted)}"
        )


def classify(result, case):
    """Require an actual, single GoogleTest execution, not merely exit status zero."""
    stdout = read_log(result.stdout)
    stderr = read_log(result.stderr)
    runs = re.findall(r"^\[ RUN      \] (.+)$", stdout, re.MULTILINE)
    started = runs == [case]
    if result.error or result.returncode is None:
        status = "infrastructure_error"
    elif result.timed_out:
        status = "timeout"
    elif result.returncode < 0 or re.search(
        r"panicked at|runner exited with signal:|runner exited with exit status: 101",
        stderr,
    ):
        status = "crash"
    elif not started:
        status = "infrastructure_error"
    else:
        terminals = re.findall(
            r"^\[ *(OK|FAILED|SKIPPED) *\] " + re.escape(case) + r"(?: \(.*\))?$",
            stdout, re.MULTILINE,
        )
        if "FAILED" in terminals and result.returncode != 0:
            status = "failure"
        elif result.returncode != 0:
            status = "crash"
        elif terminals == ["OK"]:
            status = "pass"
        elif terminals and set(terminals) == {"SKIPPED"}:
            status = "skip"
        else:
            status = "infrastructure_error"
    return {**asdict(result), "status": status, "test_started": started}


def test_environment(tmpdir):
    # Do not inherit GTEST_* or TEST_ON_GVISOR: these must exercise Linux
    # expectations, not gVisor-specific expectations or a caller's test filters.
    return {
        "PATH": os.defpath, "HOME": str(tmpdir), "TMPDIR": str(tmpdir),
        "TEST_TMPDIR": str(tmpdir), "LC_ALL": "C", "TZ": "UTC",
    }


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def prepare(manifest, work, logs, bazel, jobs, metadata):
    source = work / "gvisor"
    if not source.exists():
        source.mkdir()
        checked(["git", "init", "--quiet"], logs / "git-init", cwd=source)
        checked(
            ["git", "fetch", "--depth=1", manifest["repository"], manifest["revision"]],
            logs / "git-fetch", cwd=source,
        )
        checked(
            ["git", "checkout", "--detach", manifest["revision"]],
            logs / "git-checkout", cwd=source,
        )
    revision = checked(["git", "rev-parse", "HEAD"], logs / "git-head", cwd=source)
    dirty = checked(["git", "status", "--porcelain"], logs / "git-status", cwd=source)
    if revision != manifest["revision"] or dirty:
        raise HarnessError(f"Refusing modified or wrong-revision gVisor checkout: {source}")
    if (source / ".bazelversion").read_text().strip() != manifest["bazel_version"]:
        raise HarnessError("Manifest Bazel version differs from pinned upstream .bazelversion")
    build_env = {**os.environ, "BAZELISK_HOME": str(work / "bazelisk")}
    bazel_version = checked(
        [bazel, "--version"], logs / "bazel-version", cwd=source, env=build_env,
    )
    if bazel_version != f"bazel {manifest['bazel_version']}":
        raise HarnessError(f"Expected Bazel {manifest['bazel_version']}, got {bazel_version}")
    metadata["bazel"] = bazel_version
    checked(
        [
            bazel, "--batch", f"--output_user_root={work / 'bazel'}",
            "build", "--noshow_progress", f"--jobs={jobs}",
            *[f"//test/syscalls/linux:{test['binary']}" for test in manifest["tests"]],
        ],
        logs / "gvisor-build", cwd=source, env=build_env, timeout=1800,
    )
    target = work / "litebox"
    checked(
        [
            "cargo", "build", "--locked", "--jobs", str(jobs), "--target-dir", str(target),
            "--target", "x86_64-unknown-linux-gnu",
            "-p", "litebox_packager", "-p", "litebox_broker_userland",
            "-p", "litebox_runner_linux_userland",
        ],
        logs / "litebox-build", cwd=WORKSPACE, timeout=1800,
    )
    binaries = target / "x86_64-unknown-linux-gnu" / "debug"
    for tool, command in (
        ("rustc", ["rustc", "--version"]),
        ("cargo", ["cargo", "--version"]),
        ("compiler", ["g++", "--version"]),
        ("libc", ["getconf", "GNU_LIBC_VERSION"]),
    ):
        metadata[tool] = checked(command, logs / tool, cwd=WORKSPACE)
    metadata["litebox_binaries"] = {
        name: sha256(binaries / name) for name in (
            "litebox_packager", "litebox-broker-userland", "litebox_runner_linux_userland",
        )
    }
    return source, binaries


def package_test(binary, package, tools, logs):
    """Package the binary, its libraries and Bazel runfiles at their original paths."""
    runfiles = Path(str(binary) + ".runfiles")
    if not runfiles.is_dir():
        raise HarnessError(f"Missing Bazel runfiles directory: {runfiles}")
    inputs = [str(binary)]
    includes = []
    for path in sorted(runfiles.rglob("*")):
        if path.is_file() and path != runfiles / "MANIFEST":
            # Positional inputs discover helper ELF dependencies as well.
            with path.open("rb") as source:
                is_elf = source.read(4) == b"\x7fELF"
            if is_elf:
                inputs.append(str(path))
            else:
                includes.extend(["--include", f"{path}:{path}"])
    checked(
        [str(tools / "litebox_packager"), *inputs, "-o", str(package), *includes],
        logs, cwd=WORKSPACE,
    )


def run_test(test, source, tools, run_dir, timeout, report):
    binary = source / "bazel-bin" / "test/syscalls/linux" / test["binary"]
    directory = run_dir / test["binary"]
    directory.mkdir()
    listing = checked(
        [str(binary), "--gtest_list_tests", "--gtest_color=no"],
        directory / "listing", cwd=directory, env=test_environment(directory),
        timeout=timeout,
    )
    discovered = parse_test_list(listing)
    validate_selection(test, discovered)
    report["inventory"][test["binary"]] = discovered
    package = directory / "rootfs.tar"
    package_test(binary, package, tools, directory / "packaging")
    report["binaries"][test["binary"]] = {
        "path": str(binary), "sha256": sha256(binary),
        "rootfs": str(package), "rootfs_sha256": sha256(package),
    }
    for index, case in enumerate(test["cases"]):
        case_dir = directory / f"{index:03d}-{case}"
        case_dir.mkdir()
        arguments = [f"--gtest_filter={case}", "--gtest_color=no", "--benchmark_filter=^$"]
        pair = {"binary": test["binary"], "case": case}
        for mode in ("native", "litebox"):
            logs = case_dir / mode
            # Broker control sockets must fit sockaddr_un, regardless of how
            # deeply nested the persistent artifact directory is.
            with tempfile.TemporaryDirectory(prefix="litebox-compat-", dir="/tmp") as temporary:
                environment = test_environment(Path(temporary))
                command = [str(binary), *arguments]
                if mode == "litebox":
                    command = [
                        str(tools / "litebox-broker-userland"),
                        "--fs-initial-files", str(package),
                        "--runner", str(tools / "litebox_runner_linux_userland"),
                    ]
                    for key, value in test_environment(Path("/tmp")).items():
                        command.extend(["--env", f"{key}={value}"])
                    command.extend([str(binary), *arguments])
                result = run_process(
                    command, logs, cwd=Path(temporary), env=environment, timeout=timeout,
                )
                pair[mode] = classify(result, case)
        report["results"].append(pair)
        print(f"{case}: native={pair['native']['status']} litebox={pair['litebox']['status']}",
              flush=True)
        write_report(report, run_dir)


def report_exit_code(report):
    outcomes = [pair[mode]["status"] for pair in report["results"]
                for mode in ("native", "litebox")]
    if (not report["results"] or report["infrastructure_errors"]
            or "infrastructure_error" in outcomes):
        return 2
    if any(status not in ("pass", "skip") for status in outcomes):
        return 1
    if any(pair["native"]["status"] == "pass" and pair["litebox"]["status"] != "pass"
           for pair in report["results"]):
        return 1
    return 0


def write_report(report, directory):
    counts = {
        mode: dict(Counter(pair[mode]["status"] for pair in report["results"]))
        for mode in ("native", "litebox")
    }
    eligible = [pair for pair in report["results"] if pair["native"]["status"] == "pass"]
    report["summary"] = {
        "statuses": {mode: {status: counts[mode].get(status, 0) for status in STATUSES}
                     for mode in counts},
        "native_pass_cases": len(eligible),
        "litebox_pass_among_native_pass": sum(
            pair["litebox"]["status"] == "pass" for pair in eligible
        ),
        "excluded_cases": sum(len(test["excluded"]) for test in report["manifest"]["tests"]),
    }
    output = directory / "results.json"
    temporary = output.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(report, indent=2) + "\n")
    temporary.replace(output)
    lines = [
        "# Linux compatibility baseline", "",
        "| Binary | Case | Native | LiteBox |",
        "|---|---|---|---|",
        *[
            f"| {pair['binary']} | {pair['case']} | "
            f"{pair['native']['status']} | {pair['litebox']['status']} |"
            for pair in report["results"]
        ],
        "",
        f"Native-pass cases: {len(eligible)}; LiteBox passes among them: "
        f"{report['summary']['litebox_pass_among_native_pass']}.",
        f"Explicit exclusions: {report['summary']['excluded_cases']}. "
        "Skips are not passes; native failures are not LiteBox incompatibilities.",
        "",
    ]
    for test in report["manifest"]["tests"]:
        for case, reason in test["excluded"].items():
            lines.append(f"- Excluded `{case}`: {reason}")
    if report["infrastructure_errors"]:
        lines.extend(["", "## Infrastructure errors", ""])
        lines.extend(f"- {error}" for error in report["infrastructure_errors"])
    (directory / "summary.md").write_text("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, default=WORKSPACE / "target/linux-compat")
    parser.add_argument("--bazel", default="bazel", help="Bazel or Bazelisk executable")
    parser.add_argument("--jobs", type=int, default=4, help="Parallel Bazel build actions")
    parser.add_argument("--timeout", type=float, default=30, help="Seconds per test process")
    args = parser.parse_args()
    if args.jobs < 1 or not 0 < args.timeout < float("inf"):
        parser.error("--jobs and --timeout must be finite positive values")
    if sys.platform != "linux" or platform.machine() != "x86_64":
        parser.error("This initial subset supports x86-64 Linux only")
    if os.geteuid() == 0:
        parser.error("Run as an unprivileged user, preferably in a disposable VM")
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix="run-", dir=work))
    report = {
        "schema_version": 1, "created_at": datetime.now(timezone.utc).isoformat(),
        "manifest": json.loads(MANIFEST.read_text()),
        "metadata": {"uname": list(platform.uname()), "python": sys.version,
                     "uid": os.getuid(), "gid": os.getgid(), "guest_uid": 1000,
                     "guest_gid": 1000, "timeout_seconds": args.timeout,
                     "native_nofile_limit": list(resource.getrlimit(resource.RLIMIT_NOFILE)),
                     "umask": "0022", "harness_sha256": sha256(Path(__file__)),
                     "manifest_sha256": sha256(MANIFEST)},
        "inventory": {}, "binaries": {}, "results": [], "infrastructure_errors": [],
    }
    print(f"Artifacts: {run_dir}", flush=True)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    def interrupt(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    try:
        logs = run_dir / "setup"
        metadata = report["metadata"]
        metadata["litebox_revision"] = checked(
            ["git", "rev-parse", "HEAD"], logs / "litebox-head", cwd=WORKSPACE,
        )
        metadata["litebox_worktree"] = checked(
            ["git", "status", "--porcelain"], logs / "litebox-status", cwd=WORKSPACE,
        )
        bazel = shutil.which(args.bazel)
        if bazel is None:
            raise HarnessError(f"Bazel executable not found: {args.bazel}")
        source, tools = prepare(
            report["manifest"], work, logs, str(Path(bazel).resolve()), args.jobs, metadata,
        )
        for test in report["manifest"]["tests"]:
            try:
                run_test(test, source, tools, run_dir, args.timeout, report)
            except (HarnessError, OSError) as error:
                report["infrastructure_errors"].append(f"{test['binary']}: {error}")
                print(f"Infrastructure error: {test['binary']}: {error}", file=sys.stderr)
    except (HarnessError, OSError) as error:
        report["infrastructure_errors"].append(str(error))
        print(f"Infrastructure error: {error}", file=sys.stderr)
    except KeyboardInterrupt:
        report["infrastructure_errors"].append("Interrupted; active process group terminated")
        print("Interrupted", file=sys.stderr)
        return 130
    finally:
        write_report(report, run_dir)
        print(f"Report: {run_dir / 'summary.md'}", flush=True)
    return report_exit_code(report)


if __name__ == "__main__":
    sys.exit(main())
