"""Runs arx on a small tree and writes transcripts of the real output, for the README screenshots.

    python scripts/transcripts.py          # writes target/demo/workflow.txt and target/demo/damage.txt
"""
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARX = str(ROOT / "target" / "release" / "arx.exe")
DEMO = ROOT / "target" / "demo"
SRC = ROOT.parent  # the folder holding the portfolio projects; a few real source folders make the demo tree


def run(args, cwd, lines=None):
    """Runs arx, returns the command line and its combined output (stdout then stderr)."""
    r = subprocess.run([ARX, *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8")
    out = (r.stdout + r.stderr).replace("\r\n", "\n").rstrip("\n").split("\n")
    if lines:
        out = out[:lines]
    return "> arx " + " ".join(args), out


def main():
    shutil.rmtree(DEMO, ignore_errors=True)
    tree = DEMO / "demo"
    for name in ["24-grep-alternative/src", "24-grep-alternative/tests", "18-hex-editor/src"]:
        shutil.copytree(SRC / name, tree / name.replace("/", "-"), ignore=shutil.ignore_patterns("target", "oracle.json"))
    os.chdir(DEMO)

    t = []
    for args, lines in [
        (["create", "demo.arx", "demo", "--codec", "deflate"], None),
        (["list", "-l", "demo.arx", "demo/24-grep-alternative-src"], 6),
        (["info", "demo.arx"], None),
        (["verify", "demo.arx"], None),
    ]:
        cmd, out = run(args, DEMO, lines)
        t += [cmd, *out, ""]
    (DEMO / "workflow.txt").write_text("\n".join(t).rstrip() + "\n", encoding="utf-8")

    # Flip one byte in the middle of the archive, as a failing disk might.
    data = bytearray((DEMO / "demo.arx").read_bytes())
    data[len(data) // 2] ^= 0x20
    (DEMO / "damaged.arx").write_bytes(data)
    t = []
    for args, lines in [
        (["verify", "damaged.arx"], None),
        (["extract", "damaged.arx", "-C", "rescued", "--recover"], None),
    ]:
        cmd, out = run(args, DEMO, lines)
        t += [cmd, *out, ""]
    (DEMO / "damage.txt").write_text("\n".join(t).rstrip() + "\n", encoding="utf-8")
    print((DEMO / "workflow.txt").read_text(encoding="utf-8"))
    print("----")
    print((DEMO / "damage.txt").read_text(encoding="utf-8"))


if __name__ == "__main__":
    main()
