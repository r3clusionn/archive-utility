"""Compares arx with tar+gzip and 7-Zip on a real tree and on one large file.

    python scripts/bench.py [--runs 3]

Corpora:
    TREE  ~/.cargo/registry/src        the crates cargo has downloaded: thousands of small source files
    BIG   target/bench/rust-all.txt    every .rs file of TREE joined into one text file (see below)

BIG is made once with:
    python -c "import os,glob; ..."   (see the README) or any large text file passed as --big PATH
"""
import argparse
import hashlib
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARX = str(ROOT / "target" / "release" / "arx.exe")
SEVENZ = r"C:\Program Files\7-Zip\7z.exe"
GNU_TAR = r"C:\Windows\System32\tar.exe"  # bsdtar, the tar that ships with Windows
GZIP = r"C:\Program Files\Git\usr\bin\gzip.exe"
TREE = Path(os.path.expanduser("~/.cargo/registry/src"))
WORK = ROOT / "target" / "bench"


def run(cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, **kw)
    if r.returncode not in (0,):
        raise RuntimeError(f"{cmd[:3]} failed ({r.returncode}): {r.stderr.decode(errors='replace')[:300]}")
    return r


def timed(fn, runs):
    times = []
    for _ in range(runs):
        t = time.perf_counter()
        fn()
        times.append(time.perf_counter() - t)
    return statistics.median(times)


def tree_digest(root: Path):
    """(file count, total bytes, one digest over every path and content), to compare extractions."""
    h = hashlib.blake2b(digest_size=16)
    n = total = 0
    for dp, dn, fn in sorted((a, sorted(b), sorted(c)) for a, b, c in os.walk(root)):
        for f in fn:
            p = Path(dp) / f
            rel = str(p.relative_to(root)).replace("\\", "/")
            data = p.read_bytes()
            h.update(rel.encode() + b"\0" + hashlib.blake2b(data, digest_size=16).digest())
            n += 1
            total += len(data)
    return n, total, h.hexdigest()


def rmtree(p: Path):
    if p.exists():
        shutil.rmtree(p, ignore_errors=True)


def bench_tree(runs):
    WORK.mkdir(parents=True, exist_ok=True)
    src_bytes = sum(f.stat().st_size for f in TREE.rglob("*") if f.is_file())
    n_files = sum(1 for f in TREE.rglob("*") if f.is_file())
    print(f"== tree: {n_files} files, {src_bytes / 1e6:.0f} MB  (median of {runs} runs, files in the OS cache)\n")
    parent, name = str(TREE.parent), TREE.name
    out = WORK / "x"
    cases = [
        ("arx lz4", "arx", [ARX, "create", str(WORK / "t.arx"), str(TREE), "--codec", "lz4", "-q"], [ARX, "extract", str(WORK / "t.arx"), "-C", str(out), "-q"], [ARX, "verify", str(WORK / "t.arx")], WORK / "t.arx"),
        ("arx deflate -6", "arx", [ARX, "create", str(WORK / "t6.arx"), str(TREE), "--codec", "deflate", "-q"], [ARX, "extract", str(WORK / "t6.arx"), "-C", str(out), "-q"], [ARX, "verify", str(WORK / "t6.arx")], WORK / "t6.arx"),
        ("tar + gzip (bsdtar)", "tgz", [GNU_TAR, "-czf", str(WORK / "t.tgz"), "-C", parent, name], [GNU_TAR, "-xzf", str(WORK / "t.tgz"), "-C", str(out)], [GZIP, "-t", str(WORK / "t.tgz")], WORK / "t.tgz"),
        ("7-Zip zip deflate -mx5", "7z", [SEVENZ, "a", "-tzip", "-mm=Deflate", "-mx=5", "-mmt=on", "-bso0", "-bsp0", str(WORK / "t.zip"), str(TREE) + "\\*"], [SEVENZ, "x", "-y", "-bso0", "-bsp0", f"-o{out}", str(WORK / "t.zip")], [SEVENZ, "t", "-bso0", "-bsp0", str(WORK / "t.zip")], WORK / "t.zip"),
        ("7-Zip LZMA2 -mx1", "7z", [SEVENZ, "a", "-t7z", "-m0=LZMA2", "-mx=1", "-mmt=on", "-bso0", "-bsp0", str(WORK / "t.7z"), str(TREE) + "\\*"], [SEVENZ, "x", "-y", "-bso0", "-bsp0", f"-o{out}", str(WORK / "t.7z")], [SEVENZ, "t", "-bso0", "-bsp0", str(WORK / "t.7z")], WORK / "t.7z"),
    ]
    print(f"{'tool':<26}{'create':>9}{'size':>10}{'ratio':>8}{'extract':>9}{'check':>8}")
    for label, kind, create, extract, check, archive in cases:
        def do_create():
            archive.unlink(missing_ok=True)
            run(create)

        t_create = timed(do_create, runs)
        size = archive.stat().st_size

        def do_extract():
            rmtree(out)
            out.mkdir(parents=True)
            run(extract)

        t_extract = timed(do_extract, runs)
        # What came out must be what went in.
        extracted_root = out / name if kind in ("tgz", "arx") else out
        if kind == "arx":
            extracted_root = out / name
        got = tree_digest(extracted_root)
        want = tree_digest(TREE)
        assert got == want, f"{label}: the extracted tree differs from the source ({got} vs {want})"
        t_check = timed(lambda: run(check), runs)
        print(f"{label:<26}{t_create:>8.2f}s{size / 1e6:>8.1f}MB{size * 100 / src_bytes:>7.1f}%{t_extract:>8.2f}s{t_check:>7.2f}s")
        rmtree(out)
        archive.unlink(missing_ok=True)


def bench_big(path: Path, runs):
    size = path.stat().st_size
    print(f"\n== one file: {path.name}, {size / 1e6:.0f} MB, arx with 1 to 24 threads (median of {runs} runs)\n")
    cores = os.cpu_count() or 1
    print(f"{'threads':<9}{'lz4':>10}{'lz4 MB/s':>10}{'deflate-6':>11}{'MB/s':>7}")
    base = {}
    for j in [1, 2, 4, 8, 16, cores]:
        if j > cores:
            continue
        row = []
        for codec in ("lz4", "deflate"):
            a = WORK / f"big-{codec}.arx"
            t = timed(lambda: (a.unlink(missing_ok=True), run([ARX, "create", str(a), str(path), "--codec", codec, "-j", str(j), "-q"])), runs)
            row.append((t, a.stat().st_size))
            base.setdefault(codec, t)
        print(f"{j:<9}{row[0][0]:>9.2f}s{size / 1e6 / row[0][0]:>10.0f}{row[1][0]:>10.2f}s{size / 1e6 / row[1][0]:>7.0f}")
    for codec in ("lz4", "deflate"):
        a = WORK / f"big-{codec}.arx"
        print(f"  {codec}: {a.stat().st_size / 1e6:.1f} MB ({a.stat().st_size * 100 / size:.1f}% of the input)")
    # one-thread tool references on the same file
    gz = WORK / "big.gz"
    t = timed(lambda: (gz.unlink(missing_ok=True), run(f'"{GZIP}" -6 -c "{path}" > "{gz}"', shell=True)), max(1, runs - 1))
    print(f"  gzip -6, one thread:  {t:.2f}s ({size / 1e6 / t:.0f} MB/s), {gz.stat().st_size / 1e6:.1f} MB")
    gz.unlink(missing_ok=True)
    z = WORK / "big.zip"
    t = timed(lambda: (z.unlink(missing_ok=True), run([SEVENZ, "a", "-tzip", "-mm=Deflate", "-mx=5", "-mmt=on", "-bso0", "-bsp0", str(z), str(path)])), runs)
    print(f"  7-Zip zip deflate -mx5, threads on: {t:.2f}s ({size / 1e6 / t:.0f} MB/s), {z.stat().st_size / 1e6:.1f} MB")
    z.unlink(missing_ok=True)
    for codec in ("lz4", "deflate"):
        (WORK / f"big-{codec}.arx").unlink(missing_ok=True)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--big", default=str(WORK / "rust-all.txt"))
    ap.add_argument("--only", choices=["tree", "big"])
    a = ap.parse_args()
    if a.only != "big":
        bench_tree(a.runs)
    if a.only != "tree":
        bench_big(Path(a.big), a.runs)
