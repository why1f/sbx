"""Build optional agent/kernel combinations without committing dependency changes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

MODULE = "github.com/sagernet/sing-box"
MODULE_DIRS = ("agent", "e2e", "spike")
ARCHES = ("amd64", "arm64")
MANIFEST = "kernel-manifest.json"
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?")


def version(text, stable=False):
    value = text.strip().removeprefix("v")
    if not VERSION.fullmatch(value) or (stable and "-" in value):
        raise ValueError(f"Invalid version: {text!r}")
    return value


def asset_name(agent, kernel, arch):
    if arch not in ARCHES:
        raise ValueError("Unsupported architecture")
    return f"sbx-agent-v{version(agent, True)}-singbox-v{version(kernel)}-linux-{arch}"


def release_tag(agent, kernel):
    return f"agent-v{version(agent, True)}-singbox-v{version(kernel)}"


def choose_release(releases, channel):
    candidates = [r for r in releases if not r.get("draft") and r.get("prerelease", False) == (channel == "prerelease")]
    if not candidates:
        raise ValueError(f"No {channel} release available")
    return max(candidates, key=lambda r: r.get("published_at") or "")


class GitHub:
    def __init__(self, repository, token=""):
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
            raise ValueError("Invalid repository")
        self.repository, self.token = repository, token

    def request(self, path, method="GET", data=None, binary=False, missing=False):
        url = path if path.startswith("https://") else "https://api.github.com" + path
        headers = {"User-Agent": "sbx-kernel-builder", "Accept": "application/vnd.github+json"}
        # Never forward a repository token to public asset/CDN URLs.
        if self.token and url.startswith(("https://api.github.com/", "https://uploads.github.com/")):
            headers["Authorization"] = "Bearer " + self.token
        if data is not None:
            headers["Content-Type"] = "application/octet-stream" if binary else "application/json"
            if not binary:
                data = json.dumps(data).encode()
        try:
            with urlopen(Request(url, data=data, headers=headers, method=method), timeout=180) as response:
                return json.load(response)
        except HTTPError as exc:
            if missing and exc.code == 404:
                return None
            raise RuntimeError(f"GitHub request failed: HTTP {exc.code} ({method})") from exc

    def release(self, repository, tag):
        return self.request(f"/repos/{repository}/releases/tags/{quote(tag, safe='')}", missing=True)


def validate_manifest(manifest, metadata):
    for field in ("agent_version", "sing_box_version", "channel", "source_commit", "release_tag", "repository"):
        if manifest.get(field) != metadata[field]:
            raise ValueError(f"Existing build has mismatched {field}")
    if manifest.get("schema_version") != 1 or manifest.get("tests_passed") is not True:
        raise ValueError("Build has no successful validation manifest")
    if not isinstance(manifest.get("protocol_version"), int) or manifest["protocol_version"] < 1:
        raise ValueError("Build has no valid protocol version")
    assets = manifest.get("assets", [])
    if len(assets) != 2 or {a.get("arch") for a in assets} != set(ARCHES):
        raise ValueError("Incomplete architecture set")
    for asset in assets:
        name = asset_name(metadata["agent_version"], metadata["sing_box_version"], asset["arch"])
        expected = f"https://github.com/{metadata['repository']}/releases/download/{metadata['release_tag']}/{name}"
        if asset.get("url") != expected or not re.fullmatch(r"[0-9a-f]{64}", asset.get("sha256", "")):
            raise ValueError("Invalid asset URL/checksum")


def existing_build(api, metadata):
    release = api.release(api.repository, metadata["release_tag"])
    if release is None:
        return None
    if release["draft"]:
        raise ValueError("An unfinished draft exists; inspect/remove it before rebuilding")
    names = {a["name"] for a in release["assets"]}
    required = {MANIFEST}
    for arch in ARCHES:
        name = asset_name(metadata["agent_version"], metadata["sing_box_version"], arch)
        required.update((name, name + ".sha256"))
    if not required.issubset(names):
        raise ValueError("Existing release is incomplete; refusing to overwrite it")
    manifest_asset = next(a for a in release["assets"] if a["name"] == MANIFEST)
    manifest = api.request(manifest_asset["browser_download_url"])
    validate_manifest(manifest, metadata)
    return release


def resolve(api, channel, kernel_input="", agent_input=""):
    if channel not in ("stable", "prerelease"):
        raise ValueError("Invalid channel")
    if agent_input:
        agent = version(agent_input, True)
        base = api.release(api.repository, "v" + agent)
    else:
        base = api.request(f"/repos/{api.repository}/releases/latest")
        agent = version(base["tag_name"], True)
    if not base or base["draft"] or base["prerelease"]:
        raise ValueError("The agent base must be a published stable sbx release")
    if kernel_input:
        kernel = version(kernel_input)
        upstream = api.release("SagerNet/sing-box", "v" + kernel)
    elif channel == "stable":
        upstream = api.request("/repos/SagerNet/sing-box/releases/latest")
    else:
        upstream = choose_release(api.request("/repos/SagerNet/sing-box/releases?per_page=100"), channel)
    if not upstream or upstream["draft"] or upstream["prerelease"] != (channel == "prerelease"):
        raise ValueError("Requested kernel version does not belong to the selected channel")
    kernel = version(upstream["tag_name"])
    if ("-" in kernel) != (channel == "prerelease"):
        raise ValueError("Kernel version/channel mismatch")
    obj = api.request(f"/repos/{api.repository}/git/ref/tags/v{agent}")["object"]
    for _ in range(5):
        if obj["type"] == "commit":
            break
        if obj["type"] != "tag":
            raise ValueError("Release tag does not reference a commit")
        obj = api.request(f"/repos/{api.repository}/git/tags/{obj['sha']}")["object"]
    if obj["type"] != "commit" or not re.fullmatch(r"[0-9a-f]{40}", obj["sha"]):
        raise ValueError("Cannot resolve the immutable release commit")
    return {"schema_version": 1, "repository": api.repository, "agent_version": agent,
            "sing_box_version": kernel, "channel": channel, "source_commit": obj["sha"],
            "release_tag": release_tag(agent, kernel)}


def run(command, cwd, env=None):
    return subprocess.run(command, cwd=cwd, env=env, check=True, text=True, stdout=subprocess.PIPE).stdout.strip()


def check_source(source, metadata):
    actual = tomllib.loads((source / "master/Cargo.toml").read_text())["package"]["version"]
    if actual != metadata["agent_version"] or run(["git", "rev-parse", "HEAD"], source) != metadata["source_commit"]:
        raise ValueError("Source checkout does not match the published agent version")


def prepare(source, metadata, output):
    check_source(source, metadata)
    output.mkdir(parents=True, exist_ok=True)
    for module in MODULE_DIRS:
        directory = source / module
        run(["go", "get", MODULE + "@v" + metadata["sing_box_version"]], directory)
        run(["go", "mod", "tidy"], directory)
        selected = run(["go", "list", "-m", "-f", "{{.Version}}", MODULE], directory)
        if selected != "v" + metadata["sing_box_version"]:
            raise ValueError("Selected Go module version differs from the request")
        for filename in ("go.mod", "go.sum"):
            destination = output / module / filename
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((directory / filename).read_bytes())
    allowed = {f"{module}/{name}" for module in MODULE_DIRS for name in ("go.mod", "go.sum")}
    modified = set(run(["git", "diff", "--name-only"], source).splitlines())
    if not modified.issubset(allowed):
        raise ValueError("Kernel preparation unexpectedly modified application source")


def package(source, metadata, output):
    check_source(source, metadata)
    output.mkdir(parents=True, exist_ok=True)
    dependency_digests = {}
    for module in MODULE_DIRS:
        selected = run(["go", "list", "-m", "-f", "{{.Version}}", MODULE], source / module)
        if selected != "v" + metadata["sing_box_version"]:
            raise ValueError("Build dependencies differ from the tested kernel")
        for name in ("go.mod", "go.sum"):
            path = f"{module}/{name}"
            dependency_digests[path] = hashlib.sha256((source / path).read_bytes()).hexdigest()
    assets = []
    for arch in ARCHES:
        name = asset_name(metadata["agent_version"], metadata["sing_box_version"], arch)
        destination = output / name
        env = dict(os.environ, GOOS="linux", GOARCH=arch, CGO_ENABLED="0")
        flags = f"-s -w -buildid= -X main.Version={metadata['agent_version']} -X {MODULE}/constant.Version={metadata['sing_box_version']}"
        run(["go", "build", "-mod=readonly", "-trimpath", "-tags", "with_quic,with_utls",
             "-ldflags", flags, "-o", str(destination), "./cmd/sbx-agent"], source / "agent", env)
        digest = hashlib.sha256(destination.read_bytes()).hexdigest()
        (output / (name + ".sha256")).write_text(f"{digest}  {name}\n")
        assets.append({"arch": arch, "sha256": digest,
                       "url": f"https://github.com/{metadata['repository']}/releases/download/{metadata['release_tag']}/{name}"})
    protocol = re.search(r"PROTO_VERSION:\s*u32\s*=\s*(\d+)", (source / "shared/src/version.rs").read_text())
    if protocol is None:
        raise ValueError("Cannot determine the master/agent protocol version")
    # Only the workflow job gated by the full CI sets this marker. Local packages
    # remain unverified and cannot accidentally be published by this helper.
    manifest = dict(metadata, tests_passed=os.environ.get("KERNEL_TESTS_PASSED") == "true", protocol_version=int(protocol[1]), assets=assets,
                    dependency_sha256=dependency_digests,
                    ci_url=f"https://github.com/{metadata['repository']}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}")
    (output / MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n")


def publish(api, metadata, directory):
    existing = existing_build(api, metadata)
    if existing:
        return existing["html_url"]
    manifest = json.loads((directory / MANIFEST).read_text())
    validate_manifest(manifest, metadata)
    for asset in manifest["assets"]:
        name = asset_name(metadata["agent_version"], metadata["sing_box_version"], asset["arch"])
        if hashlib.sha256((directory / name).read_bytes()).hexdigest() != asset["sha256"]:
            raise ValueError("Artifact checksum mismatch")
        if (directory / (name + ".sha256")).read_text().split() != [asset["sha256"], name]:
            raise ValueError("Checksum file mismatch")
    body = (f"Optional agent build: sbx **{metadata['agent_version']}**, sing-box **{metadata['sing_box_version']}** "
            f"({metadata['channel']}).\n\nBase commit: `{metadata['source_commit']}`.\n\n"
            f"[Validation workflow]({manifest['ci_url']}). Dependency hashes and download metadata: `{MANIFEST}`.\n\n"
            "This build does not change the default sbx release or automatically update installed agents.")
    release = api.request(f"/repos/{api.repository}/releases", "POST", {
        "tag_name": metadata["release_tag"], "target_commitish": metadata["source_commit"],
        "name": f"sbx-agent {metadata['agent_version']} / sing-box {metadata['sing_box_version']}",
        "body": body, "draft": True, "prerelease": metadata["channel"] == "prerelease", "make_latest": "false"})
    upload = release["upload_url"].split("{")[0]
    names = [MANIFEST]
    for arch in ARCHES:
        name = asset_name(metadata["agent_version"], metadata["sing_box_version"], arch)
        names.extend((name, name + ".sha256"))
    for name in names:
        api.request(upload + "?name=" + quote(name), "POST", (directory / name).read_bytes(), binary=True)
    finished = api.request(f"/repos/{api.repository}/releases/{release['id']}", "PATCH",
                           {"draft": False, "make_latest": "false"})
    return finished["html_url"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("resolve", "prepare", "package", "publish"))
    parser.add_argument("--channel", choices=("stable", "prerelease"), default="stable")
    parser.add_argument("--kernel", default="")
    parser.add_argument("--agent", default="")
    parser.add_argument("--metadata", type=Path, default=Path("kernel-metadata.json"))
    parser.add_argument("--source", type=Path, default=Path("source"))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    api = GitHub(os.environ.get("GITHUB_REPOSITORY", "why1f/sbx"), os.environ.get("GH_TOKEN", ""))
    if args.command == "resolve":
        metadata = resolve(api, args.channel, args.kernel, args.agent)
        existing = existing_build(api, metadata)
        args.metadata.write_text(json.dumps(metadata, indent=2) + "\n")
        if output := os.environ.get("GITHUB_OUTPUT"):
            with open(output, "a", encoding="utf-8") as stream:
                for key in ("agent_version", "sing_box_version", "source_commit", "release_tag"):
                    stream.write(f"{key}={metadata[key]}\n")
                stream.write(f"exists={str(existing is not None).lower()}\n")
        print(existing["html_url"] if existing else json.dumps(metadata))
    else:
        metadata = json.loads(args.metadata.read_text())
        if args.command == "prepare":
            prepare(args.source.resolve(), metadata, args.output.resolve())
        elif args.command == "package":
            package(args.source.resolve(), metadata, args.output.resolve())
        else:
            print(publish(api, metadata, args.output.resolve()))


if __name__ == "__main__":
    main()
