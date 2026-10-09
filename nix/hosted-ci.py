"""Retain source, inputs and exact native admission qualification results."""

import hashlib
import json
import os
import shutil
import subprocess
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ALLOWED = {
    ".#packages.x86_64-linux.default",
    ".#checks.x86_64-linux.test",
    ".#checks.x86_64-linux.clippy",
    ".#nixosTests.x86_64-linux.shared-admission",
    ".#nixosTests.x86_64-linux.shared-admission-atlas-kernel",
    ".#nixosTests.x86_64-linux.supervision",
}
NATIVE_CASES = {
    "rapid native and admitted jobs preserve completion and fail closed on startup loss",
    "namespace runners share a host-reserve-backed aggregate ceiling",
    "concurrent preparation helpers share the reserve-backed runner slice",
    "durable root pool tracks every potential execution owner",
    "helper cannot omit host capacity before private entry",
    "simultaneous users, restart persistence, and automatic lending",
    "changed native enforcement inhibits a fitting smaller job",
    "cancelled pending work never executes after native cleanup",
    "short native bursts exceed the normal budget and retain restart accounting",
    "sized ordinary calls use the normal budget with bursts disabled",
    "burst client loss and private restart retain grants until cleanup is observable",
    "explicit burst cancellation confirms descendant cleanup",
    "an aged ordinary request receives a native burst quiet window",
    "advance game intent drains existing work and gates ordinary and burst entry",
    "a launch from an already-running client owns a separate native game lifetime",
    "nested exec handoff reconciles an emptied parent before consume",
    "prepared helper loss cancels the intent without executing payload",
    "prepared scope preserves a game-only filesystem namespace and surviving descendants",
    "prepared host helper preserves private PID and user namespaces",
    "bounded page return makes real swap progress without disabling swap",
    "explicit whole-device recovery restores swap on success and interrupted cleanup",
    "mixed original swap charges and offline fallback retain native backing",
    "post-grant migration is kernel-blocked on direct and swap-cache recovery paths",
    "finite completion children finish a drain while newer root worker serials wait",
}
SUPERVISION_CASES = {
    "identity-bound recovery waits for descendant cleanup and starts once",
    "chronological replay and public fail-closed heartbeat",
    "in-flight supervisor restart trips without replay or forgiven budget",
    "shadow observations have no intervention authority",
    "native recovery completes without OOM kills",
    "host admission denies inhibited and expired heartbeats",
    "cold failed backend recovers once with durable invocation accounting",
    "invalid heartbeat and missing memory observations retain live grants",
    "replacement during native recovery trips without signalling the replacement",
    "repeated native recoveries exhaust the domain budget without forgiving attempts",
    "different native domains exhaust the host budget without forgiving attempts",
}


def verify_native_report(path, required_cases=NATIVE_CASES):
    cases = ET.parse(path).getroot().findall(".//testcase")
    if not required_cases.issubset({case.get("name") for case in cases}):
        raise RuntimeError("Native VM report is missing required execution cases")
    if any(
        case.find(tag) is not None
        for case in cases
        for tag in ("failure", "error", "skipped")
    ):
        raise RuntimeError("Native VM report contains unsuccessful execution cases")


def verify_kernel_guard_evidence(guards):
    if set(guards) != {"direct", "cache", "lifetime"}:
        raise RuntimeError("Native page return lacks both guarded kernel fault paths")
    lifetime = guards["lifetime"]
    required_lifetime = ("sharedMmMigrationDenied", "conflictingGuardDenied", "cloneIntoCgroupDenied",
                         "privateForkReset", "ownerTurnoverPinnedCharge", "offliningDenied",
                         "controllerDisableDenied", "unchangedControllersAllowed", "execCookieChanged", "obsoleteMmReadEmpty", "lastCloseReleased")
    if set(lifetime) != set(required_lifetime) or any(lifetime[field] is not True for field in required_lifetime):
        raise RuntimeError("Native page return lacks shared-mm, owner, offlining or last-FD lifetime proof")
    for path in ("direct", "cache"):
        batch = guards[path]
        required = ("postScanChargeOwnerDenied", "missingGuardDenied", "duplicateGuardHeld", "targetMigrationDeniedAfterGrant",
                    "readerMigrationDeniedAfterGrant", "brokerRestartProtected", "helperLossReleased")
        expected = "directBytes" if path == "direct" else "cacheBytes"
        other = "cacheBytes" if path == "direct" else "directBytes"
        owner = "targetInode" if path == "direct" else "readerInode"
        if (batch.get("schemaVersion") != 1
            or any(batch.get(field) is not True for field in required)
            or any(type(batch.get(field)) is not int for field in
                   ("residentBytes", "directBytes", "cacheBytes", "targetInode", "readerInode", "oom", "oomKill"))
            or batch["residentBytes"] != 2 * 1048576
            or batch[expected] != 2 * 1048576 or batch[other] != 0
            or batch["targetInode"] <= 0 or batch["readerInode"] <= 0
            or batch["targetInode"] == batch["readerInode"]
            or batch.get("chargedBytesByInode") != {str(batch[owner]): 2 * 1048576}
            or batch["oom"] != 0 or batch["oomKill"] != 0):
            raise RuntimeError("Native guard lacks post-grant migration, exact fault charging or cleanup proof")


def verify_foreground_evidence(path, kernel_series=None):
    receipt = json.loads(path.read_text())
    kernel = receipt.get("kernel", "")
    if not isinstance(kernel, str) or not kernel or (kernel_series is not None and not kernel.startswith(kernel_series + ".")):
        raise RuntimeError("Native page return evidence is for the wrong kernel series")
    verify_kernel_guard_evidence(receipt.get("pageReturnGuard", {}))
    if receipt.get("outsideRunnerPreparationDenied") is not True:
        raise RuntimeError("Native preparation accepted an unbacked runner peer")
    pool = receipt.get("poolRetirement", {})
    if (pool.get("removedDomainReleased") is not True
        or pool.get("liveOwnerCleared") is not True
        or pool.get("descendantRetainedBytes") != 96 * 1048576
        or pool.get("finalBytes") != 0):
        raise RuntimeError("Native root pool lacks persisted release after policy retirement")
    abandoned = receipt.get("abandonedPreparation", {})
    if (abandoned.get("sigkillWhileBrokerOffline") is not True
        or abandoned.get("waitMilliseconds") != 3600000
        or abandoned.get("restartClearedBarrier") is not True
        or abandoned.get("existingWorkRetainedBytes") != 32 * 1048576
        or abandoned.get("payloadDidNotExecute") is not True):
        raise RuntimeError("Native preparation lacks abandoned-helper restart cleanup")
    page = receipt.get("pageReturn", {})
    device = receipt.get("deviceReturn", {})
    if device.get("postScanNativeDemandDenied") is not True:
        raise RuntimeError("Native device recovery reused stale post-scan demand")
    completion = receipt.get("completion", {})
    namespaces = receipt.get("nativeNamespaceCompletion", {})
    charge = receipt.get("chargeOwner", {})
    runners = receipt.get("namespaceRunnerBacking", {})
    responsive = receipt.get("burstManagerResponsiveness", {})
    if (responsive.get("responses") != 3
        or type(responsive.get("elapsedSeconds")) not in (int, float)
        or not 0 <= responsive["elapsedSeconds"] < 1):
        raise RuntimeError("burst manager blocked native broker RPCs")
    helpers = receipt.get("preparationHelperBacking", {})
    if (helpers.get("concurrentHelpers") != 2
        or helpers.get("aggregateBytesPerUser") != 64 * 1048576
        or helpers.get("reservedBytes") != 128 * 1048576
        or helpers.get("restartPreserved") is not True
        or helpers.get("payloadsEnteredAfterDrain") is not True
        or helpers.get("existingWorkCompleted") is not True):
        raise RuntimeError("Native preparation helpers lack concurrent aggregate backing")
    if (runners.get("aggregateBytesPerUser") != 64 * 1048576
        or runners.get("reservedBytes") != 128 * 1048576
        or runners.get("concurrentRunners") != 2
        or runners.get("innerCommittedBytes") != 64 * 1048576
        or runners.get("restartPreserved") is not True
        or runners.get("changedCeilingDenied") is not True
        or runners.get("removedUserRetained") is not True
        or runners.get("excessReserveDenied") is not True
        or runners.get("retiredUserReleasedAfterCleanup") is not True):
        raise RuntimeError("Native namespace runners lack aggregate host/native backing")
    if (page.get("replacementInvocationReclaimed") is not True
        or page.get("replacementBatchBytes") != 2 * 1048576):
        raise RuntimeError("Native recovery lacks replacement-invocation reclamation")
    if (
        charge.get("schemaVersion") != 2
        or not charge.get("kernel")
        or any(
            charge.get(key) is not True
            for key in (
                "frozenMigration",
                "originalOwnerDenied",
                "offlineFallbackDenied",
                "helperFallbackDenied",
            )
        )
        or any(
            charge.get(key) != 2 * 1048576
            for key in ("batchBytes", "fallbackResidentBytes", "onlineResidentBytes")
        )
        or charge.get("oom") != 0
        or charge.get("oomKill") != 0
        or charge.get("before", {}).get("destination", {}).get("swap") != 0
        or any(
            charge.get("before", {}).get(owner, {}).get("swap", 0) < 32 * 1048576
            or charge.get("before", {}).get(owner, {}).get("cached") != 0
            for owner in ("a", "b")
        )
    ):
        raise RuntimeError(
            "Native charge-owner evidence lacks mixed-origin, migration or fallback backing"
        )
    denial = charge.get("helperDenial", {})
    if (
        denial.get("granted") is not False
        or denial.get("waiting") != "ancestor_headroom"
        or denial.get("requestedBytes") != 8 * 1048576
        or denial.get("observationsBefore", {}).get("helper", {}).get("memory", 0)
        <= 120 * 1048576
    ):
        raise RuntimeError("Native reader fallback lacks a real unused-capacity denial")
    for name, allowed in [
        ("fallbackCharge", {"helper", "target"}),
        ("onlineCharge", {"original"}),
    ]:
        batch = charge.get(name, {})
        charged = batch.get("chargedBytes", {})
        inodes = batch.get("chargeInodes", {})
        if (
            batch.get("granted") is not True
            or batch.get("resident_bytes") != 2 * 1048576
            or batch.get("uniqueResidentBytes") != 2 * 1048576
            or not charged
            or not set(charged) <= allowed
            or any(type(value) is not int or value <= 0 for value in charged.values())
            or sum(charged.values()) != 2 * 1048576
            or any(
                type(inodes.get(owner)) is not int or inodes[owner] <= 0
                for owner in charged
            )
        ):
            raise RuntimeError(
                "Native returned pages lack per-page kpagecgroup charge attribution"
            )
    for batch in (denial, charge["fallbackCharge"], charge["onlineCharge"]):
        if (type(batch.get("inventoryStatusReplies")) is not int
            or batch["inventoryStatusReplies"] <= 0
            or not isinstance(batch.get("inventoryMaxStatusLatencyMs"), (int, float))
            or not 0 <= batch["inventoryMaxStatusLatencyMs"] < 2000):
            raise RuntimeError("Native inventory blocked ordinary broker status replies")
        if any(
            batch.get("helperEvents", {}).get(key) != "0" for key in ("oom", "oom_kill")
        ):
            raise RuntimeError("Native reader fallback lacks zero-OOM counters")
    if set(namespaces) != {"private-pid", "private-pid-user"}:
        raise RuntimeError(
            "Native completion evidence lacks the required namespace variants"
        )
    for modes in namespaces.values():
        if set(modes) != {"native", "admitted"}:
            raise RuntimeError(
                "Native completion evidence lacks both managed launch modes"
            )
        for run in modes.values():
            jobs = run.get("rapid_jobs", [])
            if (
                [r.get("exit") for r in jobs] != [0, 42, 0, 42]
                or any(r.get("streams") != "preserved" for r in jobs)
                or run.get("startup_loss")
                != {"disconnect": "no-exec", "invalid-ack": "no-exec"}
                or run.get("host_runner_loss")
                != {
                    "disconnect": "no-exec",
                    "wrong-pid": "no-exec",
                    "wrong-cgroup": "no-exec",
                }
            ):
                raise RuntimeError(
                    "Native namespace completion lacks literal streams, exit status or startup denial"
                )
    page_fields = [
        "beforeSwapBytes",
        "afterSwapBytes",
        "beforeCachedBytes",
        "afterCachedBytes",
        "beforeReturnBytes",
        "afterReturnBytes",
        "mappingBytes",
        "beforePresentBytes",
        "beforeSwappedBytes",
        "afterPresentBytes",
        "afterSwappedBytes",
    ]
    if (
        page.get("schemaVersion") != 2
        or any(type(page.get(key)) is not int or page[key] < 0 for key in page_fields)
        or page["beforeSwapBytes"] < 32 * 1048576
        or page["beforeReturnBytes"] < 32 * 1048576
        or page["beforeCachedBytes"] > page["beforeSwapBytes"]
        or page["afterCachedBytes"] > page["afterSwapBytes"]
        or page["beforeReturnBytes"]
        != page["beforeSwapBytes"] - page["beforeCachedBytes"]
        or page["afterReturnBytes"] != page["afterSwapBytes"] - page["afterCachedBytes"]
        or page["afterReturnBytes"] != 0
        or page["mappingBytes"] != 64 * 1048576
        or page["beforeSwappedBytes"] < 32 * 1048576
        or page["beforePresentBytes"] + page["beforeSwappedBytes"]
        != page["mappingBytes"]
        or page["afterPresentBytes"] != page["mappingBytes"]
        or page["afterSwappedBytes"] != 0
        or page.get("dataIntact") is not True
        or page.get("waitExitCode") != 75
        or page.get("interruptedBatchBytes") != 2 * 1048576
        or page.get("unreadBatchSettlementDenied") is not True
    ):
        raise RuntimeError(
            "Native page-return evidence lacks complete progress and safe interruption"
        )
    if (
        type(device.get("beforeUsedKiB")) is not int
        or type(device.get("afterUsedKiB")) is not int
        or not 0 <= device["afterUsedKiB"] < device["beforeUsedKiB"]
        or device.get("restoredPriority") != 10
        or device.get("interruptedRestored") is not True
        or device.get("brokerUnavailableRestored") is not True
        or device.get("wrongActivePriorityDenied") is not True
    ):
        raise RuntimeError(
            "Native device-return evidence lacks progress or interrupted restoration"
        )
    if (
        completion.get("parentAndEscrowBytes") != 128 * 1048576
        or completion.get("transferredBytes") != completion.get("parentAndEscrowBytes")
        or completion.get("postParentBytes") != 96 * 1048576
        or completion.get("outerRetainedAcrossRestart") is not True
        or completion.get("ownersFinishedWithDescendant") is not True
        or completion.get("newSerialGranted") is not False
        or completion.get("gameMemoryBytes") != 64 * 1048576
    ):
        raise RuntimeError(
            "Native completion evidence lacks backed transfer or finite worker lifetime"
        )


def verify_admission_failures(path):
    heartbeats = json.loads((path / "admission-failures.json").read_text())
    observations = json.loads((path / "admission-observations.json").read_text())
    cases = [
        "missing",
        "malformed",
        "oversized",
        "writable",
        "foreign-owner",
        "symlink",
        "expired",
        "future",
        "changed-boot",
        "inhibit",
        "degraded",
    ]
    memory_cases = [
        "missing-meminfo",
        "malformed-meminfo",
        "missing-memory-psi",
        "malformed-memory-psi",
    ]
    if [r.get("case") for r in heartbeats] != ["valid", *cases, "valid"] or [
        r.get("case") for r in observations
    ] != [*memory_cases, "valid"]:
        raise RuntimeError(
            "Native admission failure evidence is missing required variants"
        )
    retained = {}
    for result in heartbeats + observations:
        granted = {r["id"]: r for r in result["retained"] if r["granted"]}
        if any(granted.get(ticket) != entry for ticket, entry in retained.items()):
            raise RuntimeError(
                "Unavailable evidence released or replaced live commitments"
            )
        if result["committedBytes"] != sum(r["memory_bytes"] for r in granted.values()):
            raise RuntimeError(
                "Native admission commitments do not cover every granted ceiling"
            )
        if result["case"] == "valid":
            if not result["granted"] or len(granted) != len(retained) + 1:
                raise RuntimeError(
                    "Restored observations did not grant exactly one additional pool"
                )
            retained = granted
        elif (
            result["granted"]
            or granted != retained
            or result["samples"] < 2
            or result["elapsedSeconds"] < 2
            or result["waiting"] != "unknown"
        ):
            raise RuntimeError(
                "Unavailable required evidence did not deny admission across broker ticks"
            )


def verify_recovery_failures(path):
    replacement = json.loads((path / "replacement-status.json").read_text())
    original = replacement["original"]
    status = replacement["status"]
    if (
        original["phase"]["phase"] != "cooling"
        or replacement["replacementInvocation"] == original["identity"]["invocation"]
        or not status["inhibit"]
        or status["recovery"]["active"]["phase"]["phase"] != "tripped"
        or status["recovery"]["active"]["identity"] != original["identity"]
        or len(status["recovery"]["attempts"]) != 1
    ):
        raise RuntimeError(
            "Native replacement did not revoke the original recovery authority"
        )
    for name, domains, domain_limit, host_limit in [
        ("domain", ["budget-a", "budget-a"], 2, 4),
        ("host", ["budget-b", "budget-a"], 3, 2),
    ]:
        receipt = json.loads((path / (name + "-budget-status.json")).read_text())
        before, after = receipt["beforeRestart"], receipt["afterRestart"]
        completed = receipt["completedRecoveries"]
        attempts = after["recovery"]["attempts"]
        if (
            len(completed) != 2
            or [a["domain"] for a in attempts] != domains
            or before["recovery"] != after["recovery"]
            or not before["inhibit"]
            or not after["inhibit"]
            or after["recovery"]["active"]["phase"]["phase"] != "tripped"
            or after["recovery"]["active"]["identity"]["invocation"]
            != receipt["failedInvocation"]
            or after["policy"]["domain_recovery_limit"] != domain_limit
            or after["policy"]["host_recovery_limit"] != host_limit
        ):
            raise RuntimeError(
                "Native recovery exhaustion or restart persistence evidence is incomplete"
            )
        for index, state in enumerate(completed, 1):
            if (
                state["recovery"]["active"] is not None
                or state["recovery"]["attempts"] != attempts[:index]
            ):
                raise RuntimeError(
                    "Recovery budget evidence lacks independently completed native recoveries"
                )
    foreign = json.loads((path / "foreign-status.json").read_text())
    if (
        not foreign["invocation"]
        or foreign["finalInvocation"] != foreign["invocation"]
        or foreign["starts"] != 1
        or foreign["childAlive"] is not True
    ):
        raise RuntimeError(
            "Failure injection disrupted an unenrolled native invocation"
        )


def verify_supervision_evidence(path):
    for name in (
        "trace.jsonl",
        "recovery.json",
        "status.json",
        "replay.json",
        "replay-input.json",
        "oom.json",
        "heartbeat.json",
        "cold-status.json",
        "admission-failures.json",
        "admission-observations.json",
        "replacement-status.json",
        "domain-budget-status.json",
        "host-budget-status.json",
        "foreign-status.json",
    ):
        if not (path / name).is_file():
            raise RuntimeError(f"Native supervision report is missing {name}")
    binding = json.loads((path / "replay-input.json").read_text())
    trace = (path / "trace.jsonl").read_bytes()
    replay = (path / "replay.json").read_bytes()
    if binding != {
        "schemaVersion": 1,
        "traceSha256": hashlib.sha256(trace).hexdigest(),
        "traceBytes": len(trace),
        "replaySha256": hashlib.sha256(replay).hexdigest(),
    }:
        raise RuntimeError(
            "Supervision replay evidence is not bound to the exported trace and summary"
        )
    heartbeat = json.loads((path / "heartbeat.json").read_text())
    if heartbeat != [
        {"inhibit": True, "age_ms": 0, "granted": False},
        {"inhibit": False, "age_ms": 4000, "granted": False},
        {"inhibit": False, "age_ms": 0, "granted": True},
    ]:
        raise RuntimeError(
            "Host admission heartbeat evidence does not prove denial and recovery"
        )
    verify_admission_failures(path)
    verify_recovery_failures(path)
    cold = json.loads((path / "cold-status.json").read_text()).get("recovery", {})
    if (
        cold.get("active", True) is not None
        or len(cold.get("attempts", [])) != 1
        or cold["attempts"][0].get("domain") != "cold-backend"
    ):
        raise RuntimeError(
            "Cold failed backend evidence does not prove one accounted recovery"
        )
    oom = json.loads((path / "oom.json").read_text())
    if any(
        oom[phase][counter] != 0
        for phase in ("initial", "final")
        for counter in ("oom", "oom_kill", "host_oom_kill")
    ):
        raise RuntimeError("Native supervision report contains OOM events")


def retain():
    evidence = Path(os.environ["SIMIT_NIX_BUILD_RESULTS"])
    installable = (evidence / "installable").read_text().strip()
    if installable not in ALLOWED:
        raise RuntimeError("Unexpected qualification installable")
    result = json.loads((evidence / "result.json").read_text())
    if len(result) != 1 or set(result[0]["outputs"]) != {"out"}:
        raise RuntimeError("Expected exactly one selected output")
    output = Path(result[0]["outputs"]["out"])
    if not str(output).startswith("/nix/store/") or not output.exists():
        raise RuntimeError("Selected qualification output is not realized")
    revision = (evidence / "revision").read_text().strip()
    actual = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    if revision != actual:
        raise RuntimeError("Checkout changed during qualification")
    subprocess.run(
        ["git", "diff", "--exit-code", "--", "flake.lock"], cwd=ROOT, check=True
    )
    shutil.copyfile(ROOT / "flake.lock", evidence / "flake.lock")
    if "nixosTests" in installable:
        required = (
            SUPERVISION_CASES if installable.endswith(".supervision") else NATIVE_CASES
        )
        verify_native_report(output / "junit.xml", required)
        shutil.copyfile(output / "junit.xml", evidence / "junit.xml")
        if installable.endswith((".shared-admission", ".shared-admission-atlas-kernel")):
            verify_foreground_evidence(
                output / "shared-admission" / "amc-foreground-evidence.json",
                "7.2" if installable.endswith(".shared-admission-atlas-kernel") else "6.18",
            )
            shutil.copytree(output / "shared-admission", evidence / "shared-admission")
        if installable.endswith(".supervision"):
            # The VM explicitly exports these mechanism receipts. Keeping just
            # a driver PASS would lose calibration and bounded recovery evidence.
            verify_supervision_evidence(output / "supervision" / "evidence")
            shutil.copytree(output / "supervision", evidence / "supervision")
    (evidence / "qualification.json").write_text(
        json.dumps(
            {
                "schemaVersion": 1,
                "passed": True,
                "revision": revision,
                "installable": installable,
                "generatorRevision": "8008329afdadb9b2c6cd917cd4e1736243e5bc74",
                "runId": os.environ.get("GITHUB_RUN_ID"),
                "runAttempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
                "eventSha": os.environ.get("GITHUB_SHA"),
                "derivation": result[0]["drvPath"],
                "outputs": result[0]["outputs"],
                "activated": False,
            },
            indent=2,
        )
        + "\n"
    )
    print(f"Retained passing exact-source qualification for {installable}")


if __name__ == "__main__":
    retain()
