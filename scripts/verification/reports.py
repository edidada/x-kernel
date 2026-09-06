# SPDX-License-Identifier: Apache-2.0
"""Shared fail-closed acceptance checks for pinned Kani reports."""

import hashlib
from pathlib import Path
import re

CHECK = re.compile(
    r'^Check \d+: (?P<property>[^\n]+)\n'
    r'[ \t]+- Status: (?P<status>[^\n]+)\n'
    r'[ \t]+- Description: "(?P<description>.*?)"\n'
    r'(?:[ \t]+- Location: (?P<location>[^\n]+)\n)?', re.M | re.S,
)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_sources(root, config):
    production = root / config["production"]
    excluded = set(config.get("source_exclusions", []))
    sources = {str(p.relative_to(root)) for p in (production / "src").rglob("*.rs")
               if str(p.relative_to(production)) not in excluded}
    sources.add(str((production / "Cargo.toml").relative_to(root)))
    if sources != set(config["production_sha256"]):
        raise ValueError("production file set changed: review verification scope")
    for name, expected in config["production_sha256"].items():
        if digest(root / name) != expected:
            raise ValueError(f"{name}: source changed; review contracts before updating scope hash")


def audit_report(report, name, policy):
    ending = "VERIFICATION:- SUCCESSFUL"
    if policy["panic"]:
        ending += " (encountered one or more panics as expected)"
    if ending not in report.splitlines():
        raise ValueError(f"{name}: missing expected successful result")
    checks = [match.groupdict() for match in CHECK.finditer(report)]
    if not checks or len(checks) != len(re.findall(r"^Check \d+:", report, re.M)):
        raise ValueError(f"{name}: incomplete report")
    markers = {}
    failures = 0
    universal = policy.get("universal", False)
    for check in checks:
        prop, status = check["property"], check["status"]
        description = check["description"]
        if ".cover." in prop:
            if universal:
                if (not prop.startswith(name + ".cover.")
                        or description not in {"panic-domain", "panic-return"}
                        or description in markers):
                    raise ValueError(f"{name}: unexpected or duplicate panic marker")
                markers[description] = status
            elif status != "SATISFIED":
                raise ValueError(f"{name}: cover is not satisfied: {description}")
        elif status == "FAILURE":
            function = (check["location"] or "").rpartition(" in function ")[2]
            if (not policy["panic"] or ".assertion." not in prop
                    or function not in policy["functions"]):
                raise ValueError(f"{name}: unexpected failure site: {prop}")
            if "description" in policy and description != policy["description"]:
                raise ValueError(f"{name}: unexpected assertion description")
            if "assertion" in policy and prop != function + policy["assertion"]:
                raise ValueError(f"{name}: unexpected assertion number")
            failures += 1
        elif status not in {"SUCCESS", "UNREACHABLE"}:
            raise ValueError(f"{name}: unresolved check {prop}: {status}")
    if policy["panic"] and not failures:
        raise ValueError(f"{name}: no expected panic reached")
    if universal:
        if markers.get("panic-domain") != "SATISFIED":
            raise ValueError(f"{name}: illegal domain is empty or unproved")
        if markers.get("panic-return") not in {"UNREACHABLE", "UNSATISFIABLE"}:
            raise ValueError(f"{name}: normal return is possible or unproved")


def check_reports(run, config, *, filtered=False):
    expected = config["harnesses"]
    names = re.findall(r"Checking harness ([^\n ]+)\.\.\.", (run / "verification.log").read_text())
    if (not names or len(names) != len(set(names)) or not set(names) <= set(expected)
            or (not filtered and set(names) != set(expected))):
        raise ValueError("missing, duplicate or unreviewed proof harnesses")
    report_dir = run / "result_output_dir"
    if {p.name for p in report_dir.iterdir() if p.is_file()} != set(names):
        raise ValueError("report files do not match executed harnesses")
    for name in names:
        audit_report((report_dir / name).read_text(), name, expected[name])
    return {"harnesses": len(names), "universal_panics": sum(
        bool(expected[name].get("universal")) for name in names)}
