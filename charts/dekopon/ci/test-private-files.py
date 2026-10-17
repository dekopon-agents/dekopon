#!/usr/bin/env python3
"""Render broker-private projected keys and a durable OAuth record directory."""
import subprocess
from pathlib import Path

CHART = Path(__file__).resolve().parents[1]


def render(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["helm", "template", "test", str(CHART), *args],
        capture_output=True, text=True, check=False,
    )


def test_private_copy_and_record_mount() -> None:
    result = render(
        "--set", "gateway.enabled=true",
        "--set", "gateway.config.existingSecret=gateway-config",
        "--set", "gateway.catalog.existingConfigMap=catalog",
        "--set", "broker.privateFiles[0].file=app-key.pem",
        "--set", "broker.privateFiles[0].existingSecret=app-key",
        "--set", "broker.privateFiles[0].existingSecretKey=key",
    )
    assert result.returncode == 0, result.stderr
    yaml = result.stdout
    copy = yaml.index('install -m 0600 -o "$uid" -g "$gid" "/dekopon-source/private-app-key.pem" "/etc/dekopon/private/app-key.pem"')
    assert yaml.index('mkdir -m 0700 "/etc/dekopon/private"') < copy < yaml.index('chown "$uid:$gid" "/etc/dekopon/private"')
    assert 'chmod 0700 "/etc/dekopon/private"' in yaml
    assert 'rm -rf "/etc/dekopon/private"' in yaml
    assert 'name: app-key' in yaml
    assert 'path: private-app-key.pem' in yaml
    assert 'mountPath: /var/lib/dekopon/oauth2-records\n' in yaml
    assert 'subPath: broker-oauth2-records\n' in yaml
    assert 'chmod 0700 "/var/lib/dekopon/broker-oauth2-records"' in yaml
    gateway = yaml.split('        - name: gateway\n', 1)[1].split('      volumes:\n', 1)[0]
    assert 'private-app-key.pem' not in gateway
    assert 'oauth2-records' not in gateway


def test_unsafe_private_file_and_overlapping_record_subdir_refuse() -> None:
    for args in (
        ("--set", "broker.privateFiles[0].file=../key", "--set", "broker.privateFiles[0].existingSecret=key", "--set", "broker.privateFiles[0].existingSecretKey=key"),
        ("--set", "broker.oauth2Records.subdir=broker-chatgpt"),
    ):
        assert render(*args).returncode != 0


if __name__ == "__main__":
    test_private_copy_and_record_mount()
    test_unsafe_private_file_and_overlapping_record_subdir_refuse()
