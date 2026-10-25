#!/usr/bin/env python3
"""Exercise durable attachment chart mounts and render-time refusals."""
import subprocess
from pathlib import Path

CHART = Path(__file__).resolve().parents[1]


def render(*settings: str) -> subprocess.CompletedProcess[str]:
    args = ["helm", "template", "test", str(CHART),
            "--set", "gateway.enabled=true",
            "--set", "gateway.config.existingSecret=gateway-config",
            "--set", "gateway.catalog.existingConfigMap=catalog"]
    for setting in settings:
        args.extend(["--set", setting])
    return subprocess.run(args, capture_output=True, text=True, check=False)


def test_assets_mount_and_private_directory() -> None:
    result = render("gateway.journal.enabled=true", "gateway.assets.enabled=true")
    assert result.returncode == 0, result.stderr
    assert "mountPath: /var/lib/dekopon-gatewayd/assets\n" in result.stdout
    assert "subPath: gateway-assets\n" in result.stdout
    assert 'chown "$gateway_uid:$gateway_gid" "/var/lib/dekopon/gateway-assets"' in result.stdout
    assert 'chmod 0700 "/var/lib/dekopon/gateway-assets"' in result.stdout


def test_invalid_assets_settings_refuse_with_their_cause() -> None:
    for settings, message in (
        (("gateway.journal.enabled=true",), "must be enabled together"),
        (("gateway.assets.enabled=true",), "must be enabled together"),
        *(((("gateway.journal.enabled=true", "gateway.assets.enabled=true", f"gateway.assets.subdir={subdir}")), "gateway.assets.subdir clashes")
          for subdir in ("gateway-journal", "gateway-wakes", "broker-chatgpt", "broker-oauth2-records", "providers", "chatgpt", "config-check")),
        (("gateway.journal.enabled=true", "gateway.assets.enabled=true", "gateway.assets.mountPath=/var/lib/../assets"), "must be a canonical absolute path"),
        *(((("gateway.journal.enabled=true", "gateway.assets.enabled=true", f"gateway.assets.mountPath={path}")), "must not equal, contain or be inside")
          for path in ("/var/lib/dekopon-gatewayd/journal", "/var/lib/dekopon-gatewayd/journal/assets", "/var/lib/dekopon-gatewayd")),
    ):
        result = render(*settings)
        assert result.returncode != 0, settings
        assert message in result.stderr, (settings, result.stderr)


if __name__ == "__main__":
    test_assets_mount_and_private_directory()
    test_invalid_assets_settings_refuse_with_their_cause()
