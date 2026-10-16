#!/usr/bin/env python3
"""Render the live gateway projection and broker-only restart boundaries."""
import json
import subprocess
from pathlib import Path

CHART = Path(__file__).resolve().parents[1]
DIRECTORY = CHART / "ci" / "values-config-directory.yaml"


def render(*args):
    result = subprocess.run(
        ["helm", "template", "dekopon", str(CHART), *args],
        text=True, capture_output=True, check=False,
    )
    assert result.returncode == 0, result.stderr
    parsed = subprocess.run(
        ["yq", "-o=json", "-I=0", "."], input=result.stdout,
        text=True, capture_output=True, check=False,
    )
    assert parsed.returncode == 0, parsed.stderr
    docs = [json.loads(line) for line in parsed.stdout.splitlines() if line.strip()]
    deployment = next(d for d in docs if d["kind"] == "Deployment")
    pod = deployment["spec"]["template"]
    volumes = {v["name"]: v for v in pod["spec"]["volumes"]}
    gateway = next(c for c in pod["spec"]["containers"] if c["name"] == "gateway")
    broker = next(c for c in pod["spec"]["initContainers"] if c["name"] == "broker")
    init = next(c for c in pod["spec"]["initContainers"] if c["name"] == "prepare-files")
    return docs, deployment, pod, volumes, gateway, broker, init


def notes(*args):
    result = subprocess.run(
        ["helm", "install", "dekopon", str(CHART), "--dry-run=client", "--debug", *args],
        text=True, capture_output=True, check=False,
    )
    assert result.returncode == 0, result.stderr
    return result.stdout.split("\nNOTES:\n", 1)[1]


def mounts(container):
    return {m["name"]: m for m in container["volumeMounts"]}


def test_live_gateway_directory_and_nested_skills():
    _, deployment, pod, volumes, gateway, broker, init = render("-f", str(DIRECTORY))
    assert gateway["args"] == ["--config", "/etc/dekopon-gateway"]
    assert mounts(gateway)["gateway-config"] == {
        "name": "gateway-config", "mountPath": "/etc/dekopon-gateway", "readOnly": True,
    }
    assert volumes["gateway-config"] == {
        "name": "gateway-config", "configMap": {"name": "dekopon-gatewayd-d", "defaultMode": 0o444},
    }
    assert mounts(gateway)["skills"] == {
        "name": "skills", "mountPath": "/etc/dekopon-skills", "readOnly": True,
    }
    assert volumes["skills"]["configMap"]["items"] == [
        {"key": "review-skill", "path": "pull-request-review/SKILL.md"},
        {"key": "review-reference", "path": "pull-request-review/references/review.cjs"},
    ]
    for name in ("gateway-config", "skills", "catalog"):
        assert "subPath" not in mounts(gateway)[name]
        assert mounts(gateway)[name]["readOnly"] is True
        assert name not in mounts(broker) and name not in mounts(init)
    assert "config" not in mounts(gateway) and "config-source" not in mounts(gateway)
    assert "gateway-config" not in mounts(broker)
    assert "skills-source" not in volumes
    assert "/dekopon-skills-source" not in init["args"][0]
    for name in ("gateway-config", "skills"):
        projection = volumes[name]["configMap"]
        assert projection["defaultMode"] & 0o022 == 0
        assert projection["defaultMode"] & 0o004 == 0o004
    assert "fsGroup" not in pod["spec"]["securityContext"]
    assert "reloader.stakater.com/search" not in (pod["metadata"].get("annotations") or {})
    assert (deployment["metadata"].get("annotations") or {}) == {}


def test_inline_gateway_source_changes_without_broker_roll():
    base = ["--set", "gateway.enabled=true", "--set", "gateway.catalog.inline=agent-a"]
    def gateway_config(value):
        return render(*base, "--set", "gateway.config.inline=broker: {serverUid: 65532}\nstopWords: [" + value + "]")
    first = gateway_config("before")
    second = gateway_config("after")
    for docs, _, pod, volumes, gateway, broker, init in (first, second):
        assert gateway["args"] == ["--config", "/etc/dekopon-gateway/gatewayd.yaml"]
        assert volumes["gateway-config"]["secret"]["items"] == [
            {"key": "gatewayd.yaml", "path": "gatewayd.yaml"},
        ]
        assert volumes["gateway-config"]["secret"]["defaultMode"] & 0o022 == 0
        assert volumes["gateway-config"]["secret"]["defaultMode"] & 0o004 == 0o004
        assert mounts(gateway)["gateway-config"]["readOnly"] is True
        assert "subPath" not in mounts(gateway)["gateway-config"]
        assert "gateway-config" not in mounts(broker) and "gateway-config" not in mounts(init)
        assert "gatewayd.yaml" not in init["args"][0]
        assert "gatewayd.yaml" not in str(volumes.get("config-source", {}))
        assert not any("gatewayd.yaml" in d.get("stringData", {}) and "broker.yaml" in d.get("stringData", {}) for d in docs)
        assert "checksum/catalog" not in (pod["metadata"].get("annotations") or {})
    def source(docs):
        return next(d["stringData"]["gatewayd.yaml"] for d in docs if "gatewayd.yaml" in d.get("stringData", {}))
    assert source(first[0]) != source(second[0])
    assert first[2]["metadata"].get("annotations") == second[2]["metadata"].get("annotations")
    assert "before" in source(first[0]) and "after" in source(second[0])
    catalog_changed = render("--set", "gateway.enabled=true", "--set", "gateway.catalog.inline=agent-b", "--set", "gateway.config.inline=broker: {serverUid: 65532}")
    assert "checksum/catalog" not in (catalog_changed[2]["metadata"].get("annotations") or {})
    assert catalog_changed[2]["metadata"].get("annotations") == first[2]["metadata"].get("annotations")


def test_restart_notice_names_only_broker_secret_sources():
    gateway_args = (
        "--set", "gateway.enabled=true", "--set", "gateway.config.existingSecret=gateway-authored",
        "--set", "gateway.catalog.existingConfigMap=agents",
    )
    assert "rollout restart" not in notes(*gateway_args)
    broker_args = ("--set", "broker.config.inline=", "--set", "broker.config.existingSecret=broker-authored")
    assert "rollout restart" in notes(*broker_args)
    assert "rollout restart" in notes(*gateway_args, *broker_args)


def test_existing_gateway_secret_and_broker_restart_sources():
    _, _, _, volumes, gateway, broker, init = render(
        "--set", "gateway.enabled=true", "--set", "gateway.config.existingSecret=gateway-authored",
        "--set", "gateway.config.existingSecretKey=host.yaml",
        "--set", "gateway.catalog.existingConfigMap=agents",
    )
    assert volumes["gateway-config"]["secret"]["secretName"] == "gateway-authored"
    assert volumes["gateway-config"]["secret"]["items"] == [
        {"key": "host.yaml", "path": "gatewayd.yaml"},
    ]
    assert "gateway-config" not in mounts(broker) and "gateway-config" not in mounts(init)
    assert "config-source" not in mounts(gateway)
    gateway_args = ("--set", "gateway.enabled=true", "--set", "gateway.config.existingSecret=gateway-authored", "--set", "gateway.catalog.existingConfigMap=agents")
    original = render(*gateway_args)
    changed = render(*gateway_args, "--set", "broker.config.inline=apiVersion: dekopon.dev/brokerd/v1alpha1\nidentities: [{uid: 65532}]")
    assert original[2]["metadata"]["annotations"]["checksum/config"] != changed[2]["metadata"]["annotations"]["checksum/config"]
    credentials = render(*gateway_args, "--set", "broker.credentials.inline=credentials: []")
    assert "checksum/credentials" in credentials[2]["metadata"]["annotations"]
    directory = render("-f", str(DIRECTORY), "--set-json", 'deploymentAnnotations={"reloader.stakater.com/search":"true"}')
    assert directory[1]["metadata"]["annotations"]["reloader.stakater.com/search"] == "true"
    assert "checksum/config" not in (directory[2]["metadata"].get("annotations") or {})
    assert "host.yaml" not in directory[4]["args"]
    assert directory[3]["broker-config-directory"]["configMap"]["name"] == "dekopon-broker-d"
    assert mounts(directory[6])["broker-config-directory"]["readOnly"] is True
    assert "*.yaml" in directory[6]["args"][0]
    assert 'install -m 0600 -o "$uid"' in directory[6]["args"][0]
    assert "/etc/dekopon/broker.d" in directory[6]["args"][0]
    assert "broker-config-directory" not in mounts(directory[4])
    assert volumes["config-source"]["projected"]["defaultMode"] == 400


if __name__ == "__main__":
    test_live_gateway_directory_and_nested_skills()
    test_inline_gateway_source_changes_without_broker_roll()
    test_restart_notice_names_only_broker_secret_sources()
    test_existing_gateway_secret_and_broker_restart_sources()
