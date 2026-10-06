import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class StageImageContextTests(unittest.TestCase):
    def test_each_variant_stages_only_its_own_verified_binaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            releases = root / "releases"
            releases.mkdir()
            for variant in ("stock", "wasmtime-optimization"):
                prefix = "dekopon" if variant == "stock" else f"dekopon-{variant}"
                for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
                    name = f"{prefix}-0.34.0-{target}"
                    archive = releases / f"{name}.tar.gz"
                    with tarfile.open(archive, "w:gz") as output:
                        for binary in ("dekopon-brokerd", "dekopon-gatewayd"):
                            data = f"{variant}:{target}:{binary}".encode()
                            member = tarfile.TarInfo(f"{name}/{binary}")
                            member.size = len(data)
                            output.addfile(member, io.BytesIO(data))
                    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                    archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n")
            gh = root / "gh"
            gh.write_text('''#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import sys
args = sys.argv[1:]
if args[:2] == ["release", "download"]:
    destination = Path(args[args.index("--dir") + 1])
    for index, arg in enumerate(args):
        if arg == "--pattern":
            for source in Path(os.environ["TEST_RELEASES"]).glob(args[index + 1]):
                shutil.copy2(source, destination)
elif args[:2] == ["attestation", "verify"]:
    with open(os.environ["TEST_VERIFIED"], "a") as output:
        output.write(Path(args[2]).name + "\\n")
else:
    sys.exit(1)
''')
            gh.chmod(0o755)
            for variant in ("stock", "wasmtime-optimization"):
                with self.subTest(variant=variant):
                    work = root / variant
                    verified = root / f"{variant}.verified"
                    env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
                               TEST_RELEASES=str(releases), TEST_VERIFIED=str(verified))
                    arguments = [str(ROOT / "ci/stage-image-context.sh"), "v0.34.0", str(work)]
                    if variant != "stock":
                        arguments.append(variant)
                    subprocess.run(arguments, env=env, check=True, capture_output=True, text=True)
                    self.assertEqual(len(verified.read_text().splitlines()), 2)
                    for binary in (work / "context/dist").glob("*/*"):
                        self.assertTrue(binary.read_text().startswith(f"{variant}:"))
                    subprocess.run(["sha256sum", "--check", str(work / "binaries.sha256")],
                                   cwd=work / "context", check=True, capture_output=True)

    def test_unknown_variant_is_rejected_before_downloading(self):
        result = subprocess.run([str(ROOT / "ci/stage-image-context.sh"), "v0.34.0", "/unused", "other"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("unsupported image variant", result.stderr)


if __name__ == "__main__":
    unittest.main()
