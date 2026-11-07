import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import kernel_variant as kv


FIXTURE = Path(__file__).parent / "testdata/kernel-manifest.json"


def metadata():
    return {k: v for k, v in json.loads(FIXTURE.read_text()).items()
            if k in ("schema_version", "repository", "agent_version", "sing_box_version", "channel", "source_commit", "release_tag")}


class FakeGitHub:
    repository = "why1f/sbx"

    def __init__(self):
        self.existing = None
        self.calls = []
        self.latest_agent_tag = "v0.4.38"
        self.agent_commit = "1" * 40
        self.manifest = json.loads(FIXTURE.read_text())

    def release(self, repo, tag):
        self.calls.append((repo, tag))
        if tag.startswith("agent-"):
            return self.existing
        return {"tag_name": tag, "draft": False, "prerelease": "-" in tag}

    def request(self, path, method="GET", data=None, **kwargs):
        self.calls.append((path, method))
        if path == "/repos/why1f/sbx/releases/latest":
            return self.release(self.repository, self.latest_agent_tag)
        if path == "/repos/SagerNet/sing-box/releases/latest":
            return self.release("SagerNet/sing-box", "v1.14.4")
        if path == "/repos/SagerNet/sing-box/releases?per_page=100":
            return [dict(self.release("SagerNet/sing-box", tag), published_at=date)
                    for tag, date in [("v1.14.4", "2026-10-24T00:00:00Z"),
                                      ("v1.15.0-alpha.12", "2026-10-11T00:00:00Z"),
                                      ("v1.15.0-alpha.13", "2026-10-24T00:01:00Z")]]
        if "/git/ref/tags/" in path:
            return {"object": {"type": "tag", "sha": "2" * 40}}
        if "/git/tags/" in path:
            return {"object": {"type": "commit", "sha": self.agent_commit}}
        if path.endswith("kernel-manifest.json"):
            return self.manifest
        if method == "POST" and path.endswith("/releases"):
            self.created = data
            return {"id": 7, "upload_url": "https://uploads.github.com/repos/why1f/sbx/releases/7/assets{?name}"}
        if method == "POST" and path.startswith("https://uploads.github.com/"):
            return {}
        if method == "PATCH":
            self.final = data
            return {"html_url": "https://github.com/why1f/sbx/releases/tag/variant"}
        raise AssertionError(path)


class KernelBuildTests(unittest.TestCase):
    def test_new_official_agent_release_changes_the_base_and_variant_name(self):
        api = FakeGitHub()
        before = kv.resolve(api, "stable", "1.14.4")
        api.latest_agent_tag = "v0.4.39"
        api.agent_commit = "3" * 40
        after = kv.resolve(api, "stable", "1.14.4")
        self.assertEqual(after["agent_version"], "0.4.39")
        self.assertEqual(after["source_commit"], "3" * 40)
        self.assertEqual(after["release_tag"], "agent-v0.4.39-singbox-v1.14.4")
        self.assertEqual(kv.asset_name(after["agent_version"], after["sing_box_version"], "amd64"),
                         "sbx-agent-v0.4.39-singbox-v1.14.4-linux-amd64")
        self.assertNotEqual(before["release_tag"], after["release_tag"])
        self.assertIn(("/repos/why1f/sbx/git/ref/tags/v0.4.39", "GET"), api.calls)

    def test_blank_version_chooses_the_selected_channel_and_peels_agent_tag(self):
        api = FakeGitHub()
        stable = kv.resolve(api, "stable")
        self.assertEqual(stable["sing_box_version"], "1.14.4")
        self.assertEqual(stable["source_commit"], "1" * 40)
        preview = kv.resolve(api, "prerelease")
        self.assertEqual(preview["sing_box_version"], "1.15.0-alpha.13")
        self.assertNotEqual(stable["release_tag"], preview["release_tag"])

    def test_explicit_version_must_belong_to_the_selected_channel(self):
        api = FakeGitHub()
        self.assertEqual(kv.resolve(api, "stable", "v1.14.2")["sing_box_version"], "1.14.2")
        with self.assertRaises(ValueError):
            kv.resolve(api, "stable", "1.15.0-alpha.13")
        with self.assertRaises(ValueError):
            kv.resolve(api, "prerelease", "1.14.4")
        for text in ("1.14", "1.14.4/../../x", "--flag", "1.14.4\nnext", "v1.14.4+build"):
            with self.assertRaises(ValueError):
                kv.version(text)

    def test_manifest_and_asset_names_match_the_master_contract(self):
        manifest = json.loads(FIXTURE.read_text())
        kv.validate_manifest(manifest, metadata())
        self.assertEqual(kv.asset_name("0.4.38", "1.14.4", "arm64"), "sbx-agent-v0.4.38-singbox-v1.14.4-linux-arm64")
        for field, value in (("source_commit", "3" * 40), ("channel", "prerelease"), ("tests_passed", False)):
            bad = copy.deepcopy(manifest)
            bad[field] = value
            with self.assertRaises(ValueError):
                kv.validate_manifest(bad, metadata())
        bad = copy.deepcopy(manifest)
        bad["assets"][0]["url"] = "https://example.com/agent"
        with self.assertRaises(ValueError):
            kv.validate_manifest(bad, metadata())

    def test_published_combination_is_reused_but_drafts_or_partial_builds_are_not(self):
        api = FakeGitHub()
        names = [kv.MANIFEST]
        for arch in kv.ARCHES:
            name = kv.asset_name("0.4.38", "1.14.4", arch)
            names.extend((name, name + ".sha256"))
        api.existing = {"draft": False, "html_url": "published", "assets": [
            {"name": name, "browser_download_url": "https://example.invalid/" + name} for name in names]}
        self.assertEqual(kv.existing_build(api, metadata())["html_url"], "published")
        api.existing["draft"] = True
        with self.assertRaises(ValueError):
            kv.existing_build(api, metadata())
        api.existing["draft"] = False
        api.existing["assets"].pop()
        with self.assertRaises(ValueError):
            kv.existing_build(api, metadata())

    def test_package_and_publish_preserve_latest_and_use_exact_dependency_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "source", root / "dist"
            (source / "master").mkdir(parents=True)
            (source / "master/Cargo.toml").write_text('[package]\nversion="0.4.38"\n')
            (source / "shared/src").mkdir(parents=True)
            (source / "shared/src/version.rs").write_text('pub const PROTO_VERSION: u32 = 1;')
            for module in kv.MODULE_DIRS:
                (source / module).mkdir()
                (source / module / "go.mod").write_text("frozen dependencies")
                (source / module / "go.sum").write_text("frozen sums")
            calls = []

            def runner(command, cwd, env=None):
                calls.append(command)
                if command[:2] == ["git", "rev-parse"]:
                    return "1" * 40
                if command[:2] == ["go", "list"]:
                    return "v1.14.4"
                if command[:2] == ["go", "build"]:
                    self.assertIn("-mod=readonly", command)
                    Path(command[command.index("-o") + 1]).write_bytes(env["GOARCH"].encode())
                    return ""
                raise AssertionError(command)

            with patch.object(kv, "run", runner), patch.dict(kv.os.environ, {"KERNEL_TESTS_PASSED": "true"}):
                kv.package(source, metadata(), output)
            manifest = json.loads((output / kv.MANIFEST).read_text())
            self.assertEqual(len(manifest["dependency_sha256"]), 6)
            self.assertEqual(len(list(output.iterdir())), 5)
            self.assertTrue(all("push" not in call and "commit" not in call for call in calls))
            api = FakeGitHub()
            kv.publish(api, metadata(), output)
            self.assertEqual(api.created["target_commitish"], "1" * 40)
            self.assertTrue(api.created["draft"])
            self.assertFalse(api.final["draft"])
            self.assertEqual(api.final["make_latest"], "false")
            (output / kv.asset_name("0.4.38", "1.14.4", "amd64")).write_bytes(b"tampered")
            with self.assertRaises(ValueError):
                kv.publish(FakeGitHub(), metadata(), output)


if __name__ == "__main__":
    unittest.main()
