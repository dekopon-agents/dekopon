#!/usr/bin/env python3
"""Render live projections and the restart boundaries for init copies."""
import json
import shutil
import subprocess
import tempfile
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
    with tempfile.TemporaryDirectory(prefix="dekopon-notes-") as temporary:
        chart = Path(temporary) / "chart"
        shutil.copytree(CHART, chart)
        templates = chart / "templates"
        text = (templates / "NOTES.txt").read_text()
        (templates / "notes-probe.yaml").write_text(
            '{{- define "dekopon.testNotes" -}}\n' + text + '\n{{- end }}\n'
            'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: notes-probe\ndata:\n'
            '  notes: {{ include "dekopon.testNotes" . | quote }}\n'
        )
        result = subprocess.run(
            ["helm", "template", "dekopon", str(chart), "--show-only", "templates/notes-probe.yaml", *args],
            text=True, capture_output=True, check=False,
        )
        assert result.returncode == 0, result.stderr
        parsed = subprocess.run(
            ["yq", "-o=json", "-I=0", "."], input=result.stdout,
            text=True, capture_output=True, check=False,
        )
        assert parsed.returncode == 0, parsed.stderr
        return json.loads(parsed.stdout)["data"]["notes"]


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


def test_restart_notice_names_only_copied_sources():
    gateway_args = (
        "--set", "gateway.enabled=true", "--set", "gateway.config.existingSecret=gateway-authored",
        "--set", "gateway.catalog.existingConfigMap=agents",
    )
    assert "rollout restart" not in notes(*gateway_args)
    broker_args = ("--set", "broker.config.inline=", "--set", "broker.config.existingSecret=broker-authored")
    assert "rollout restart" not in notes(*gateway_args, *broker_args)
    assert "rollout restart" in notes("-f", str(DIRECTORY))


def test_broker_projections_and_init_copies():
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
    args = (
        "--set", "gateway.enabled=true", "--set", "gateway.config.existingSecret=gateway-authored",
        "--set", "gateway.catalog.existingConfigMap=agents",
        "--set", "broker.policies.inline=permit(principal\\, action\\, resource);",
        "--set", "broker.secretMap.existingSecret=map", "--set", "broker.secretMap.existingSecretKey=authored.yaml",
        "--set-json", 'broker.secretBootstrapFiles=[{"file":"vault-token","existingSecret":"vault","existingSecretKey":"token"}]',
        "--set-json", 'broker.privateFiles=[{"file":"app-key","existingSecret":"app","existingSecretKey":"key"}]',
        "--set", "broker.chatgpt.enabled=true", "--set", "broker.chatgpt.existingSecret=broker-chatgpt",
    )
    docs, _, original, volumes, gateway, broker, init = render(*args)
    inline = next(d["stringData"]["broker.yaml"] for d in docs if "broker.yaml" in d.get("stringData", {}))
    changed = render(*args, "--set-json", "broker.config.inline=" + json.dumps(inline + "\nstrict: true\n"))[2]
    assert original["metadata"].get("annotations") == changed["metadata"].get("annotations")
    assert "checksum/config" not in (original["metadata"].get("annotations") or {})
    name = "broker-config-projection"
    assert mounts(broker)[name] == {"name": name, "mountPath": "/etc/dekopon/.@live", "readOnly": True}
    assert name not in mounts(gateway) and name not in mounts(init)
    live = volumes[name]["projected"]
    assert live["defaultMode"] == 0o444
    assert [(s["secret"]["items"][0]["key"], s["secret"]["items"][0]["path"]) for s in live["sources"]] == [
        ("broker.yaml", "broker.yaml"), ("policies.cedar", "policies.cedar"), ("authored.yaml", "secret-map.yaml"),
    ]
    copies = volumes["config-source"]["projected"]
    assert copies["defaultMode"] == 400
    assert [s["secret"]["items"][0]["path"] for s in copies["sources"]] == [
        "vault-token", "broker-chatgpt-auth.json", "private-app-key",
    ]
    script = init["args"][0]
    for key in ("broker.yaml", "policies.cedar", "secret-map.yaml"):
        assert f'ln -s ".@live/{key}" "/etc/dekopon/{key}"' in script
        assert f'/dekopon-source/{key}' not in script
        assert f'stat -c \'%u:%g:%a:%h:%F\' "/etc/dekopon/{key}"' not in script
    assert '"/dekopon-source/vault-token" "/etc/dekopon/vault-token"' in script
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
    live = directory[3]["broker-config-projection"]["projected"]
    assert live["defaultMode"] == 0o444
    assert [s["secret"]["items"][0]["path"] for s in live["sources"]] == ["secret-map.yaml"]

    existing = [{"name": "broker-config-projection", "mountPath": "/vault", "volume": {"secret": {"secretName": "vault"}}}]
    _, _, _, volumes, _, broker, _ = render(*args, "--set-json", "broker.secretSourceVolumes=" + json.dumps(existing))
    assert volumes["broker-config-projection"]["secret"] == existing[0]["volume"]["secret"]
    assert mounts(broker)["broker-config-projection-1"]["mountPath"] == "/etc/dekopon/.@live"


def test_retired_source_names_and_paths_are_not_reserved():
    retired = [
        {"name": "skills-source", "mountPath": "/dekopon-skills-source", "volume": {"secret": {"secretName": "a"}}},
        {"name": "gateway-config-directory", "mountPath": "/dekopon-gateway-d-source", "volume": {"secret": {"secretName": "b"}}},
    ]
    _, _, _, volumes, gateway, broker, init = render(
        "-f", str(DIRECTORY), "--set-json", "broker.secretSourceVolumes=" + json.dumps(retired),
    )
    for source in retired:
        assert volumes[source["name"]]["secret"] == source["volume"]["secret"]
        assert mounts(broker)[source["name"]]["mountPath"] == source["mountPath"]
        assert source["name"] not in mounts(gateway) and source["name"] not in mounts(init)
    render("-f", str(DIRECTORY), "--set", "providerStorage.enabled=true", "--set", "providerStorage.rootPath=/dekopon-skills-source")
    render("-f", str(DIRECTORY), "--set", "providerStorage.enabled=true", "--set", "providerStorage.rootPath=/dekopon-gateway-d-source")


def test_config_check_mounts_gateway_candidates_as_the_deployment_does():
    item = {"key": "review-skill", "path": "pull-request-review/SKILL.md", "mode": 0o664}
    docs, _, _, deployment_volumes, _, _, _ = render(
        "-f", str(DIRECTORY),
        "--set-json", "gateway.skills.items=" + json.dumps([item]),
        "--set", "configCheck.enabled=true",
        "--set", "configCheck.gateway.configMap=dekopon-check-gatewayd-d",
        "--set", "configCheck.gateway.catalogConfigMap=dekopon-check-agents-d",
        "--set", "configCheck.gateway.skillsConfigMap=dekopon-check-skills",
        "--set", "configCheck.gateway.againstConfigMap=dekopon-gatewayd-d",
    )
    job = next(d for d in docs if d["kind"] == "Job" and d["metadata"]["name"].endswith("-config-check"))
    spec = job["spec"]["template"]["spec"]
    volumes = {v["name"]: v for v in spec["volumes"]}
    check = next(c for c in spec["containers"] if c["name"] == "check-gateway")
    init = next(c for c in spec["initContainers"] if c["name"] == "prepare-candidate")
    assert check["args"][:4] == ["check", "/check/gatewayd.d", "--catalog=/check/agents.d", "--probe"]
    expected = {
        "gateway-source": "/check/gatewayd.d",
        "catalog-source": "/check/agents.d",
        "against-source": "/check/live.d",
        "skills": "/etc/dekopon-skills",
    }
    for name, path in expected.items():
        assert mounts(check)[name] == {"name": name, "mountPath": path, "readOnly": True}
        assert name not in mounts(init)
        assert "configMap" in volumes[name]
    assert "check" not in mounts(check)
    assert volumes["skills"]["configMap"] == {
        **deployment_volumes["skills"]["configMap"], "name": "dekopon-check-skills",
    }
    assert volumes["skills"]["configMap"]["items"] == [item]
    assert volumes["gateway-source"]["configMap"]["defaultMode"] == deployment_volumes["gateway-config"]["configMap"]["defaultMode"]
    assert "/check/gatewayd.d" not in init["args"][0] and "/source/skills" not in init["args"][0]


def test_against_config_mounts_the_live_catalog_from_the_deployment_source():
    for source in (
        ["--set", "gateway.catalog.existingConfigMap=live-agents", "--set", "gateway.catalog.existingConfigMapKey=live.yaml", "--set", "gateway.catalog.fileName=agents.yaml"],
        ["--set", "gateway.catalog.existingConfigMap=live-agents", "--set", "gateway.catalog.existingConfigMapKey="],
        ["--set", "gateway.catalog.existingConfigMap=", "--set", "gateway.catalog.inline=agents: []"],
    ):
        for against in ("", "live-gateway"):
            docs, _, _, deployment_volumes, _, _, _ = render(
                "-f", str(DIRECTORY), *source,
                "--set", "paths.catalogDir=/custom/live-catalog",
                "--set", "configCheck.enabled=true",
                "--set", "configCheck.gateway.configMap=candidate-gateway",
                "--set", "configCheck.gateway.catalogConfigMap=candidate-agents",
                "--set", "configCheck.gateway.againstConfigMap=" + against,
            )
            job = next(d for d in docs if d["kind"] == "Job")
            spec = job["spec"]["template"]["spec"]
            check = next(c for c in spec["containers"] if c["name"] == "check-gateway")
            volumes = {v["name"]: v for v in spec["volumes"]}
            if against:
                assert mounts(check)["catalog"] == {
                    "name": "catalog", "mountPath": "/custom/live-catalog", "readOnly": True,
                }
                assert volumes["catalog"] == deployment_volumes["catalog"]
            else:
                assert "catalog" not in mounts(check)
                assert "catalog" not in volumes


if __name__ == "__main__":
    test_live_gateway_directory_and_nested_skills()
    test_inline_gateway_source_changes_without_broker_roll()
    test_restart_notice_names_only_copied_sources()
    test_broker_projections_and_init_copies()
    test_retired_source_names_and_paths_are_not_reserved()
    test_config_check_mounts_gateway_candidates_as_the_deployment_does()

    test_against_config_mounts_the_live_catalog_from_the_deployment_source()
