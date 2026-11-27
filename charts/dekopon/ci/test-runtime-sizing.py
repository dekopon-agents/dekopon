#!/usr/bin/env python3
"""Render runtime flags for serving and operator commands."""
import json
import subprocess
from pathlib import Path

CHART = Path(__file__).resolve().parents[1]
DIRECTORY = CHART / "ci" / "values-config-directory.yaml"
FLAGS = {
    "workerThreads": ("worker-threads", "2"),
    "blockingThreads": ("blocking-threads", "8"),
    "blockingKeepAlive": ("blocking-keep-alive", "250ms"),
}


def render(*args, values=DIRECTORY):
    command = ["helm", "template", "dekopon", str(CHART)]
    if values is not None:
        command.extend(["-f", str(values)])
    rendered = subprocess.run(
        [*command, *args],
        text=True, capture_output=True, check=False,
    )
    assert rendered.returncode == 0, rendered.stderr
    parsed = subprocess.run(
        ["yq", "-o=json", "-I=0", "."], input=rendered.stdout,
        text=True, capture_output=True, check=False,
    )
    assert parsed.returncode == 0, parsed.stderr
    return [json.loads(line) for line in parsed.stdout.splitlines() if line.strip()]


def containers(docs):
    result = {}
    for doc in docs:
        if doc["kind"] not in ("Deployment", "Job"):
            continue
        pod = doc["spec"]["template"]["spec"]
        for container in pod.get("containers", []) + pod.get("initContainers", []):
            result[container["name"]] = container
    return result


def runtime_flags(arguments):
    prefixes = ("--worker-threads=", "--blocking-threads=", "--blocking-keep-alive=",
                "--cpu-threads=", "--shell-threads=")
    return [argument for argument in arguments if argument.startswith(prefixes)]


def test_each_optional_value_and_unset_defaults():
    base = containers(render())
    assert base["gateway"]["args"] == ["--config", "/etc/dekopon-gateway"]
    assert base["broker"]["args"] == ["--config", "/etc/dekopon/broker.d"]
    for daemon, extra in (("gateway", "shell"), ("broker", "cpu")):
        fields = FLAGS | {extra + "Threads": (extra + "-threads", "3")}
        for key, (flag, value) in fields.items():
            setting = f"{daemon}.runtime.{key}"
            unchanged = containers(render("--set", f"{setting}=null"))
            assert unchanged[daemon]["args"] == base[daemon]["args"]
            changed = containers(render("--set", f"{setting}={value}"))
            assert changed[daemon]["args"] == base[daemon]["args"] + [f"--{flag}={value}"]
            other = "broker" if daemon == "gateway" else "gateway"
            assert changed[other]["args"] == base[other]["args"]


def test_runtime_values_reach_every_daemon_invocation():
    settings = [
        "--set", "state.existingClaim=dekopon-state",
        "--set", "broker.providerSet.enabled=true",
        "--set", "broker.providerSync.enabled=true",
        "--set", "broker.providerSync.configMap=providers",
        "--set", "configCheck.enabled=true",
        "--set", "configCheck.broker.configMap=check-broker",
        "--set", "configCheck.gateway.configMap=check-gateway",
        "--set", "configCheck.gateway.catalogConfigMap=check-agents",
    ]
    for daemon, extra in (("gateway", "shell"), ("broker", "cpu")):
        fields = FLAGS | {extra + "Threads": (extra + "-threads", "3")}
        for key, (_, value) in fields.items():
            settings += ["--set", f"{daemon}.runtime.{key}={value}"]
    rendered = containers(render(*settings))
    for daemon, names, extra in (("gateway", ("gateway", "check-gateway"), "shell"),
                                 ("broker", ("broker", "precompile", "sync", "check-broker"), "cpu")):
        expected = sorted(f"--{flag}={value}" for flag, value in
                          (FLAGS | {extra + "Threads": (extra + "-threads", "3")}).values())
        for name in names:
            assert sorted(runtime_flags(rendered[name]["args"])) == expected, name
        if daemon == "broker":
            for probe in ("startupProbe", "readinessProbe"):
                assert sorted(runtime_flags(rendered["broker"][probe]["exec"]["command"])) == expected
    for name, command in (("precompile", "provider"), ("sync", "provider"),
                          ("check-broker", "check"), ("check-gateway", "check")):
        arguments = rendered[name]["args"]
        assert all(argument.startswith("--") for argument in arguments[:arguments.index(command)])


def test_only_broker_declares_file_termination_policy():
    with_gateway_and_config_check = render(
        "--set", "configCheck.enabled=true",
        "--set", "configCheck.broker.configMap=check-broker",
        "--set", "configCheck.gateway.configMap=check-gateway",
        "--set", "configCheck.gateway.catalogConfigMap=check-agents",
    )
    for rendered in (render(values=None), with_gateway_and_config_check):
        policies = {
            name: container["terminationMessagePolicy"]
            for name, container in containers(rendered).items()
            if "terminationMessagePolicy" in container
        }
        assert policies == {"broker": "File"}, policies


def test_broker_reads_the_selected_secret_key_as_the_complete_config_file():
    for key, args in (
        ("broker.yaml", ()),
        ("complete-config.yaml", ("--set", "broker.config.existingSecretKey=complete-config.yaml")),
    ):
        docs = render(
            "--set", "broker.config.inline=",
            "--set", "broker.config.existingSecret=broker-config",
            *args, values=None,
        )
        deployment = next(doc for doc in docs if doc["kind"] == "Deployment")
        pod = deployment["spec"]["template"]["spec"]
        source = next(volume for volume in pod["volumes"] if volume["name"] == "config-source")
        assert source["projected"]["sources"] == [
            {"secret": {"name": "broker-config", "items": [{"key": key, "path": "broker.yaml"}]}}
        ]
        rendered = containers(docs)
        assert '"/dekopon-source/broker.yaml" "/etc/dekopon/broker.yaml"' in rendered["prepare-files"]["args"][0]
        assert rendered["broker"]["args"] == ["--config", "/etc/dekopon/broker.yaml"]


if __name__ == "__main__":
    test_each_optional_value_and_unset_defaults()
    test_runtime_values_reach_every_daemon_invocation()
    test_only_broker_declares_file_termination_policy()
    test_broker_reads_the_selected_secret_key_as_the_complete_config_file()
    print("Runtime values render their flags; unset values preserve arguments.")
