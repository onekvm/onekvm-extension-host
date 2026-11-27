#!/usr/bin/env python3
"""Exercise real host preflight, package-job and stdin entry points in a disposable root."""

import argparse
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import threading
from functools import partial


PACKAGE = "onekvm-extension-ipks-probe"
VERSION = "2.0.0"
BASE = "data/onekvm/apps/ipks-probe"
MARKER = "run/onekvm/allow-unsigned-extension-packages"
LEGACY = "var/lib/onekvm/extensions/state/ipks-probe.json"
STATUS = "var/lib/opkg/status"
STATE = "etc/onekvm/extensions/ipks-probe.json"


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def add(tar, name, data):
    info = tarfile.TarInfo(name)
    info.mode = 0o644
    info.size = len(data)
    tar.addfile(info, io.BytesIO(data))


def make_ipk(directory, package, version, files, extra=""):
    def archive(entries):
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode="w:gz") as tar:
            for name, data in entries.items():
                add(tar, "./" + name, data)
        return out.getvalue()

    control = f"Package: {package}\nVersion: {version}-r0\nArchitecture: all\n{extra}"
    members = {
        "debian-binary": b"2.0\n",
        "control.tar.gz": archive({"control": control.encode()}),
        "data.tar.gz": archive(files),
    }
    output = directory / f"{package}_{version}-r0_all.ipk"
    with output.open("wb") as handle:
        handle.write(b"!<arch>\n")
        for name, data in members.items():
            header = f"{name + '/':16}{0:12}{0:6}{0:6}{100644:8}{len(data):10}`\n"
            handle.write(header.encode())
            handle.write(data)
            if len(data) % 2:
                handle.write(b"\n")
    return output


def make_plugin(directory, version, dependency=True):
    manifest = {
        "manifest_version": 1, "api_version": 2,
        "id": "ipks-probe", "name": "IPKS probe", "version": version,
        "kind": "environment",
    }
    fields = f"Depends: {PACKAGE}-runtime\n" if dependency else ""
    return make_ipk(directory, PACKAGE, version, {
        f"{BASE}/{version}/manifest.json": json.dumps(manifest).encode(),
    }, fields)


def run(*arguments, **kwargs):
    return subprocess.run(arguments, check=True, **kwargs)


def make_key(home, uid):
    home.mkdir(mode=0o700)
    gpg = ["gpg", "--batch", "--no-options", "--homedir", str(home)]
    run(*gpg, "--pinentry-mode", "loopback", "--passphrase", "",
        "--quick-generate-key", uid, "nistp256", "sign", "0")
    public = home / "public.asc"
    write(public, subprocess.check_output([*gpg, "--armor", "--export", uid]))
    return ["--signing-home", str(home), "--signing-key", uid, "--public-key", str(public)]


def unpack(bundle):
    with tarfile.open(bundle) as tar:
        return {member.name: tar.extractfile(member).read() for member in tar if member.isfile()}


def repack(path, files):
    with tarfile.open(path, "w:gz") as tar:
        for name, data in files.items():
            add(tar, name, data)


def check(root, fixtures, packer):
    app = make_plugin(fixtures, VERSION)
    dependency = make_ipk(fixtures, PACKAGE + "-runtime", VERSION, {
        f"{BASE}/{VERSION}/local.txt": b"Bundled dependency\n",
    })
    old_version = "1.0.0"
    old_app = make_plugin(fixtures, old_version, dependency=False)
    trusted = fixtures / "trusted.ipks"
    legacy = fixtures / "unsigned-v1.ipks"
    wrong = fixtures / "wrong-key.ipks"
    old = fixtures / "baseline.ipks"
    feed = fixtures / "feed"
    feed.mkdir(exist_ok=True)
    online_app = make_plugin(feed, "3.0.0", dependency=False)
    with tempfile.TemporaryDirectory(prefix="ipks-keys-") as keys:
        signing = Path(keys)
        try:
            good_args = make_key(signing / "good", "IPKS test publisher <ipks-test@invalid>")
            bad_args = make_key(signing / "bad", "Another publisher <ipks-wrong@invalid>")
            for options, output, payloads in [
                (good_args, trusted, [app, dependency]),
                (bad_args, wrong, [app, dependency]),
                (good_args, old, [old_app]),
                (["--unsigned-development"], legacy, [app, dependency]),
            ]:
                run("python3", packer, *options, str(output), PACKAGE, *map(str, payloads))
            for relative, home in [
                ("etc/opkg/gpg", "good"), ("run/onekvm/opkg-gpg", "bad"),
            ]:
                for name in ("pubring.kbx", "trustdb.gpg"):
                    path = root / relative / name
                    write(path, (signing / home / name).read_bytes())
                    path.chmod(0o600)
            embedded_key = (signing / "bad/public.asc").read_bytes()
            key = ["gpg", "--batch", "--homedir", str(signing / "bad")]
            fingerprint = next(line.split(":")[9] for line in subprocess.check_output(
                [*key, "--with-colons", "--fingerprint"], text=True,
            ).splitlines() if line.startswith("fpr:"))
            index = feed / "Packages"
            write(index, (
                f"Package: {PACKAGE}\nVersion: 3.0.0-r0\nArchitecture: all\n"
                f"Filename: {online_app.name}\nSize: {online_app.stat().st_size}\n"
                f"SHA256sum: {hashlib.sha256(online_app.read_bytes()).hexdigest()}\n\n"
            ).encode())
            run(*key, "--yes", "--armor", "--detach-sign", "--digest-algo", "SHA256",
                "--output", str(feed / "Packages.asc"), str(index))
        finally:
            for home in ("good", "bad"):
                subprocess.run(["gpgconf", "--homedir", str(signing / home), "--kill", "all"])

    files = unpack(trusted)
    root_name = "packages/" + app.name
    dependency_name = "packages/" + dependency.name
    manifest = json.loads(files["bundle.json"])
    changed = dict(files)
    changed[dependency_name] += b"tampered"
    repack(fixtures / "tampered-dependency.ipks", changed)
    changed["bundle.json"] = json.dumps({
        **manifest,
        "packages": [
            {**item, "sha256": hashlib.sha256(changed["packages/" + item["file"]]).hexdigest()}
            for item in manifest["packages"]
        ],
    }).encode()
    repack(fixtures / "rehashed-payload.ipks", changed)
    repack(fixtures / "missing-signature.ipks",
           {name: data for name, data in files.items() if name != "bundle.json.asc"})
    repack(fixtures / "extra-package.ipks", {**files, "packages/extra.ipk": files[root_name]})
    repack(fixtures / "embedded-key.ipks", {**unpack(wrong), "publisher.asc": embedded_key})
    repack(fixtures / "misordered.ipks", {
        "bundle.json": files["bundle.json"], root_name: files[root_name],
        "bundle.json.asc": files["bundle.json.asc"], dependency_name: files[dependency_name],
    })
    write(fixtures / "trailing-data.ipks", trusted.read_bytes() + wrong.read_bytes())

    def host(*args, data=None, error=None):
        res = subprocess.run(
            [shutil.which("chroot") or "/usr/sbin/chroot", str(root),
             "/usr/bin/onekvm-extension", *args],
            input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30,
        )
        if error is not None:
            assert res.returncode != 0 and error in res.stderr.decode(), res
        else:
            assert res.returncode == 0, res
        return res.stdout

    upload = "/var/tmp/onekvm/extension-uploads/" + "b" * 32 + ".upload"
    job = "e" * 32
    jobdir = root / "run/onekvm/extension-jobs"

    def staged(bundle, error=None, mutate=True):
        write(root / upload[1:], bundle.read_bytes())
        preview = host("inspect-local-path", upload, error=error)
        if error is None:
            assert json.loads(preview)["package_name"] == PACKAGE
        if not mutate:
            return
        write(jobdir / (job + ".json"), json.dumps({
            "args": ["install-local-path", upload, "--enable-after-install=false"],
        }).encode())
        host("job-run", job)
        result = json.loads((jobdir / (job + ".result")).read_bytes())
        assert result["ok"] == (error is None) and (error is None or error in result["error"]), result

    staged(old)
    # A working installation and legacy state must survive rejected uploads.
    app_manifest = root / BASE / old_version / "manifest.json"
    before = [app_manifest.read_bytes(), (root / STATUS).read_bytes(), (root / STATE).read_bytes()]
    write(root / LEGACY, b'{"enabled":false}')
    bus_log = root / "systemd.log"
    actions = bus_log.read_bytes()
    invalid = {
        "tampered-dependency.ipks": "SHA-256 verification",
        "rehashed-payload.ipks": "publisher signature verification failed",
        "wrong-key.ipks": "publisher signature verification failed",
        "embedded-key.ipks": "publisher signature verification failed",
        "missing-signature.ipks": "publisher signature is missing",
        "misordered.ipks": "publisher signature is missing",
        "extra-package.ipks": "package list does not match",
        "trailing-data.ipks": "unexpected data after the IPKS gzip stream",
        "unsigned-v1.ipks": "unsigned extension package",
        app.name: "unsigned extension package",
    }
    for name, error in invalid.items():
        fixture = fixtures / name
        staged(fixture, error)
        host("install-local", data=fixture.read_bytes(), error=error)
        assert [app_manifest.read_bytes(), (root / STATUS).read_bytes(),
                (root / STATE).read_bytes()] == before
        assert not (root / BASE / VERSION).exists()
        assert (root / LEGACY).is_file()
        assert bus_log.read_bytes() == actions
        print(f"PASS {name}: preflight, Web job, CLI, installed state and no D-Bus action")

    marker = root / MARKER
    marker.symlink_to("/etc/opkg/gpg/pubring.kbx")
    staged(legacy, "unsigned extension package", mutate=False)
    marker.unlink()
    write(marker, b"")
    marker.chmod(0o666)
    staged(legacy, "unsigned extension package", mutate=False)
    marker.chmod(0o600)
    staged(legacy, mutate=False)
    staged(old_app, mutate=False)
    staged(fixtures / "missing-signature.ipks", "publisher signature is missing")
    staged(wrong, "publisher signature verification failed")
    host("install-local", "--enable-after-install=false", data=legacy.read_bytes())
    assert (root / BASE / VERSION / "local.txt").read_bytes() == b"Bundled dependency\n"
    marker.unlink()
    staged(legacy, "unsigned extension package", mutate=False)
    staged(trusted)
    assert (root / BASE / VERSION / "local.txt").read_bytes() == b"Bundled dependency\n"
    assert not (root / LEGACY).exists()
    print("PASS trusted Web install, explicit development CLI install, and authorization removal")

    # Make the same key that failed offline verification trusted for its own
    # online source, then let the host refresh and install via real opkg.
    handler = partial(http.server.SimpleHTTPRequestHandler, directory=str(feed))
    with http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/"
            headers = f"# OneKVM-Source-Fingerprint: {fingerprint}\n"
            headers += "".join(
                f"# OneKVM-Source-Public-Key: {line}\n"
                for line in embedded_key.decode().splitlines()
            )
            config = root / "etc/opkg/onekvm-ext-ipks-test.conf"
            write(config, (headers + f"src ipks-test {url}\n").encode())
            config.chmod(0o644)
            write(root / "usr/share/onekvm/trusted-feed-prefixes", b"")
            (root / "etc/onekvm/network-policy.json").unlink()
            host("install", "ipks-probe", "--enable-after-install=false")
            assert (root / BASE / "3.0.0/manifest.json").is_file()
            staged(wrong, "publisher signature verification failed")
            print("PASS signed online index and payload installed with a configured source key")
        finally:
            server.shutdown()
            thread.join()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--packer", type=Path, required=True)
    args = parser.parse_args()
    if not (args.root / ".onekvm-disposable-ipks-test").is_file():
        parser.error("root must be a prepared disposable test root")
    args.fixtures.mkdir(parents=True, exist_ok=True)
    check(args.root.resolve(), args.fixtures.resolve(), args.packer.resolve())
