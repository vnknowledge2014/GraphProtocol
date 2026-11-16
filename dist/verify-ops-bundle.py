#!/usr/bin/env python3
"""Validate Prometheus, OpenTelemetry, and Grafana observability artifacts (`OPS-010`, `OPS-020`, `G-20`)."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent


def verify_prometheus() -> tuple[int, int]:
    alerts_path = ROOT / "ops" / "prometheus" / "alerts.yml"
    data = yaml.safe_load(alerts_path.read_text(encoding="utf-8"))
    groups = data.get("groups", [])
    if len(groups) < 2:
        raise AssertionError(f"Expected >=2 Prometheus rule groups, found {len(groups)}")

    recording_count = 0
    alert_count = 0
    raw_tenant_re = re.compile(r"\bby\s*\([^)]*\btenant\b(?![_])")

    for group in groups:
        for rule in group.get("rules", []):
            expr = str(rule.get("expr", "")).strip()
            if not expr:
                raise AssertionError(f"Empty expr in rule: {rule}")
            if raw_tenant_re.search(expr):
                raise AssertionError(f"Cardinality violation (raw `tenant` label) in expr: {expr}")
            if "record" in rule:
                name = rule["record"]
                if not str(name).startswith("gap:"):
                    raise AssertionError(f"Recording rule must start with `gap:`: {name}")
                recording_count += 1
            elif "alert" in rule:
                name = rule["alert"]
                if "for" not in rule:
                    raise AssertionError(f"Alert {name} missing `for` duration")
                labels = rule.get("labels", {})
                annotations = rule.get("annotations", {})
                for req_label in ("severity", "component", "spec_ref"):
                    if not labels.get(req_label):
                        raise AssertionError(f"Alert {name} missing label `{req_label}`")
                for req_ann in ("summary", "description", "runbook_url"):
                    if not annotations.get(req_ann):
                        raise AssertionError(f"Alert {name} missing annotation `{req_ann}`")
                runbook_rel = annotations["runbook_url"]
                if not (ROOT / runbook_rel).is_file():
                    raise AssertionError(f"Alert {name} runbook missing on disk: {runbook_rel}")
                alert_count += 1
            else:
                raise AssertionError(f"Unknown rule entry: {rule}")

    if recording_count == 0 or alert_count == 0:
        raise AssertionError("Missing Prometheus recording or alert rules")
    return recording_count, alert_count


def verify_otel() -> int:
    otel_path = ROOT / "ops" / "otel" / "collector.yaml"
    data = yaml.safe_load(otel_path.read_text(encoding="utf-8"))
    for key in ("receivers", "processors", "service"):
        if key not in data or not data[key]:
            raise AssertionError(f"OTel collector config missing `{key}`")
    pipelines = data["service"].get("pipelines", {})
    if not pipelines:
        raise AssertionError("OTel collector config has empty `service.pipelines`")
    return len(pipelines)


def verify_grafana() -> int:
    grafana_dir = ROOT / "ops" / "grafana"
    dashboards = sorted(grafana_dir.glob("*.json"))
    if len(dashboards) < 3:
        raise AssertionError(f"Expected >=3 Grafana dashboards, found {len(dashboards)}")
    total_panels = 0
    for db_path in dashboards:
        doc = json.loads(db_path.read_text(encoding="utf-8"))
        panels = doc.get("panels", [])
        if not panels:
            raise AssertionError(f"Grafana dashboard {db_path.name} has zero panels")
        total_panels += len(panels)
    return total_panels


def main() -> int:
    rec_cnt, alert_cnt = verify_prometheus()
    otel_pipes = verify_otel()
    panel_cnt = verify_grafana()
    print(
        f"OK: ops bundle verified ({rec_cnt} Prometheus recording rules, "
        f"{alert_cnt} alerts with existing runbooks, {otel_pipes} OTel pipelines, "
        f"{panel_cnt} Grafana panels across ops/grafana/*.json)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
