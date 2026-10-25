#!/usr/bin/env python3
"""`scripts/release_notes.py` 纯函数层单测。

覆盖范围（不跑 git、不跑 main）:
  parse_conventional / is_release_commit / has_breaking_footer / classify /
  select_prev_tag / parse_commits / render_notes

两种跑法都要通:
  python3 scripts/tests/test_release_notes.py -v
  python3 -m unittest discover -s scripts/tests -t scripts -p 'test_release_notes.py' -v
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

# 直跑（sys.path[0] = scripts/tests）与 discover（顶层 = scripts）都要能
# `import release_notes`，统一把 scripts/ 目录塞进 sys.path。
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import release_notes as rn  # noqa: E402

# 固定假 SHA，避免测试依赖真实仓库。
SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40


class ParseConventionalTest(unittest.TestCase):
    def test_with_scope(self) -> None:
        parsed = rn.parse_conventional("feat(ui): x")
        self.assertEqual(
            parsed,
            rn.Conventional(type="feat", scope="ui", subject="x", breaking=False),
        )

    def test_without_scope(self) -> None:
        parsed = rn.parse_conventional("fix: x")
        self.assertIsNotNone(parsed)
        assert parsed is not None  # for type checkers
        self.assertEqual(parsed.type, "fix")
        self.assertIsNone(parsed.scope)
        self.assertEqual(parsed.subject, "x")
        self.assertFalse(parsed.breaking)

    def test_breaking_marker(self) -> None:
        parsed = rn.parse_conventional("feat(api)!: x")
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.type, "feat")
        self.assertEqual(parsed.scope, "api")
        self.assertEqual(parsed.subject, "x")
        self.assertTrue(parsed.breaking)

    def test_chinese_subject(self) -> None:
        parsed = rn.parse_conventional("refactor(analytics): 重构 X")
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.type, "refactor")
        self.assertEqual(parsed.scope, "analytics")
        self.assertEqual(parsed.subject, "重构 X")

    def test_empty_scope_is_normalized_to_none(self) -> None:
        """`feat(): x` 的空 scope 归一为 None（渲染时不输出 `()`）。"""
        parsed = rn.parse_conventional("feat(): x")
        self.assertIsNotNone(parsed)
        assert parsed is not None  # for type checkers
        self.assertIsNone(parsed.scope)
        self.assertEqual(parsed.subject, "x")

    def test_merge_commit_is_not_conventional(self) -> None:
        self.assertIsNone(rn.parse_conventional("Merge pull request #12"))

    def test_empty_string_is_not_conventional(self) -> None:
        self.assertIsNone(rn.parse_conventional(""))


class IsReleaseCommitTest(unittest.TestCase):
    def test_plain_chore_release(self) -> None:
        self.assertTrue(rn.is_release_commit("chore: release v0.7.0"))

    def test_scoped_chore_release(self) -> None:
        self.assertTrue(rn.is_release_commit("chore(release): release v0.7.0"))

    def test_non_chore_type_is_not_release(self) -> None:
        self.assertFalse(rn.is_release_commit("fix: release v0.7.0"))

    def test_missing_version_is_not_release(self) -> None:
        self.assertFalse(rn.is_release_commit("chore: release notes"))


class ClassifyTest(unittest.TestCase):
    def test_known_types_map_to_their_own_group(self) -> None:
        self.assertEqual(rn.classify("feat"), "feature")
        self.assertEqual(rn.classify("fix"), "fix")
        self.assertEqual(rn.classify("refactor"), "refactor")
        self.assertEqual(rn.classify("docs"), "docs")

    def test_unlisted_common_types_go_to_misc(self) -> None:
        for type_ in ("build", "ci", "chore", "test"):
            with self.subTest(type_=type_):
                self.assertEqual(rn.classify(type_), "misc")

    def test_uppercase_type_classifies_but_keeps_case(self) -> None:
        """`classify()` 对 type 大小写宽容；但解析结果保留原文大小写。"""
        self.assertEqual(rn.classify("FEAT"), "feature")
        self.assertEqual(rn.classify("Fix"), "fix")
        parsed = rn.parse_conventional("FEAT(ui): x")
        self.assertIsNotNone(parsed)
        assert parsed is not None  # for type checkers
        self.assertEqual(parsed.type, "FEAT", msg="解析层不应改写大小写")

    def test_unknown_type_goes_to_misc(self) -> None:
        self.assertEqual(rn.classify("perf"), "misc")

    def test_none_goes_to_other(self) -> None:
        self.assertEqual(rn.classify(None), "other")

    def test_group_keys_are_render_layer_keys(self) -> None:
        """classify 的输出必须是 GROUP_ORDER 里的组键，渲染才不会 KeyError。"""
        group_keys = {key for key, _ in rn.GROUP_ORDER}
        for type_ in ("feat", "fix", "refactor", "docs", "build", None):
            with self.subTest(type_=type_):
                self.assertIn(rn.classify(type_), group_keys)


class SelectPrevTagTest(unittest.TestCase):
    TAGS = ["v0.2.0", "v0.5.0", "v0.6.0", "v0.7.0"]

    def test_skipped_versions_pick_previous_existing_tag(self) -> None:
        # v0.5.0 的直接前驱是 v0.2.0（中间版本不存在）。
        self.assertEqual(rn.select_prev_tag(self.TAGS, "v0.5.0"), "v0.2.0")

    def test_latest_tag_picks_immediate_predecessor(self) -> None:
        self.assertEqual(rn.select_prev_tag(self.TAGS, "v0.7.0"), "v0.6.0")

    def test_oldest_tag_has_no_predecessor(self) -> None:
        self.assertIsNone(rn.select_prev_tag(self.TAGS, "v0.2.0"))

    def test_non_version_tags_are_filtered(self) -> None:
        tags = ["vnext", "v1.2", "nightly", "v0.2.0", "v0.5.0", "v0.6.0"]
        self.assertEqual(rn.select_prev_tag(tags, "v0.5.0"), "v0.2.0")
        self.assertIsNone(rn.select_prev_tag(["vnext", "v1.2"], "v0.2.0"))

    def test_current_tag_is_excluded(self) -> None:
        self.assertEqual(rn.select_prev_tag(["v0.6.0", "v0.7.0"], "v0.7.0"), "v0.6.0")
        self.assertIsNone(rn.select_prev_tag(["v0.7.0"], "v0.7.0"))

    def test_unordered_input(self) -> None:
        tags = ["v0.7.0", "v0.2.0", "v0.6.0", "v0.5.0"]
        self.assertEqual(rn.select_prev_tag(tags, "v0.7.0"), "v0.6.0")
        self.assertEqual(rn.select_prev_tag(tags, "v0.5.0"), "v0.2.0")

    def test_semver_numeric_order_not_lexical(self) -> None:
        tags = ["v0.9.0", "v0.10.0"]
        self.assertEqual(rn.select_prev_tag(tags, "v0.10.0"), "v0.9.0")


class ParseCommitsTest(unittest.TestCase):
    def test_parses_record_stream(self) -> None:
        raw = (
            f"{SHA_A}\x1ffeat(ui): x\x1f\x1e\n"
            f"{SHA_B}\x1ffix: y\x1fbody line\x1e\n"
        )
        self.assertEqual(
            rn.parse_commits(raw),
            [
                rn.Commit(sha=SHA_A, subject="feat(ui): x", body=""),
                rn.Commit(sha=SHA_B, subject="fix: y", body="body line"),
            ],
        )

    def test_multi_line_body_is_kept_verbatim(self) -> None:
        raw = f"{SHA_A}\x1ffeat: x\x1fbody para\n\nBREAKING CHANGE: drop v1\n\x1e"
        parsed = rn.parse_commits(raw)
        self.assertEqual(len(parsed), 1)
        self.assertEqual(parsed[0].subject, "feat: x")
        self.assertIn("BREAKING CHANGE: drop v1", parsed[0].body)

    def test_bad_records_are_skipped_without_raising(self) -> None:
        raw = (
            f"{SHA_A}\x1ffeat: x\x1f\x1e"
            "garbage-without-separator\x1e"
            "\x1fonly-subject\x1e"
            f"{SHA_B}\x1f\x1f\x1e"
            f"{SHA_C}\x1ffix: z\x1e"
        )
        self.assertEqual(
            rn.parse_commits(raw),
            [
                rn.Commit(sha=SHA_A, subject="feat: x", body=""),
                rn.Commit(sha=SHA_C, subject="fix: z", body=""),
            ],
        )

    def test_order_is_preserved(self) -> None:
        raw = "".join(
            f"{sha}\x1fsubject {i}\x1f\x1e"
            for i, sha in enumerate([SHA_C, SHA_B, SHA_A])
        )
        parsed = rn.parse_commits(raw)
        self.assertEqual([c.sha for c in parsed], [SHA_C, SHA_B, SHA_A])

    def test_empty_input(self) -> None:
        self.assertEqual(rn.parse_commits(""), [])


class HasBreakingFooterTest(unittest.TestCase):
    def test_breaking_change_footer(self) -> None:
        self.assertTrue(rn.has_breaking_footer("body\n\nBREAKING CHANGE: drop v1"))

    def test_breaking_change_hyphen_footer(self) -> None:
        self.assertTrue(rn.has_breaking_footer("BREAKING-CHANGE: drop v1"))

    def test_body_without_footer_is_not_breaking(self) -> None:
        self.assertFalse(rn.has_breaking_footer(""))
        self.assertFalse(rn.has_breaking_footer("plain body\n\nRefs: #12"))

    def test_lowercase_token_is_not_a_footer(self) -> None:
        """conventional commits 规定 footer token 全大写，小写不当作 breaking。"""
        self.assertFalse(rn.has_breaking_footer("breaking change: drop v1"))
        self.assertFalse(rn.has_breaking_footer("Breaking-Change: drop v1"))

    def test_breaking_footer_with_crlf_body(self) -> None:
        """CRLF body（未在 `git()` 处归一化时）也要命中 footer。"""
        self.assertTrue(rn.has_breaking_footer("intro\r\nBREAKING CHANGE: drop\r\n"))
        self.assertTrue(rn.has_breaking_footer("BREAKING-CHANGE: drop\r\n"))
        self.assertFalse(rn.has_breaking_footer("plain body\r\nRefs: #12\r\n"))

    def test_token_must_start_the_line(self) -> None:
        self.assertFalse(rn.has_breaking_footer("not a BREAKING CHANGE: here"))

    def test_footer_anywhere_in_body_counts(self) -> None:
        body = "long description\n\nBREAKING CHANGE: drop v1\n\nRefs: #12"
        self.assertTrue(rn.has_breaking_footer(body))


class RenderNotesTest(unittest.TestCase):
    def test_full_body_with_prev_tag_and_repo(self) -> None:
        commits = [
            rn.Commit(sha=SHA_A, subject="feat(ui): add thing"),
            rn.Commit(sha=SHA_B, subject="fix: bug"),
        ]
        body = rn.render_notes(
            commits,
            version="v0.7.0",
            prev_tag="v0.6.0",
            date="2024-05-06",
            repo="owner/name",
        )
        expected = (
            "## sag v0.7.0 (2024-05-06)\n"
            "\n"
            "自 [`v0.6.0`](https://github.com/owner/name/releases/tag/v0.6.0)"
            " 以来共 2 个提交。\n"
            "\n"
            "### ✨ 新增\n"
            "- feat(ui): add thing ([`aaaaaaa`]"
            "(https://github.com/owner/name/commit/"
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa))\n"
            "\n"
            "### 🐛 修复\n"
            "- fix: bug ([`bbbbbbb`]"
            "(https://github.com/owner/name/commit/"
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb))\n"
            "\n"
            "---\n"
            "\n"
            "**完整变更日志**: [`v0.6.0...v0.7.0`]"
            "(https://github.com/owner/name/compare/v0.6.0...v0.7.0)\n"
        )
        self.assertEqual(body, expected)

    def test_uppercase_type_renders_head_verbatim(self) -> None:
        """大写 type 进「新增」组，但渲染行保留原文大写（不归一为小写）。"""
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="FEAT(ui): x")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("### ✨ 新增", body)
        self.assertIn("- FEAT(ui): x", body)
        self.assertNotIn("- feat(ui): x", body)

    def test_group_order_is_fixed(self) -> None:
        # 输入故意打乱，输出必须按 GROUP_ORDER 排列。
        commits = [
            rn.Commit(sha=SHA_A, subject="docs: readme"),
            rn.Commit(sha=SHA_B, subject="chore: bump deps"),
            rn.Commit(sha=SHA_C, subject="feat: feature"),
        ]
        body = rn.render_notes(
            commits, version="v0.7.0", prev_tag=None, date="2024-05-06"
        )
        positions = [
            body.index(f"### {title}")
            for title in ("✨ 新增", "🔧 杂项")
        ]
        self.assertEqual(positions, sorted(positions))

    def test_empty_groups_are_omitted(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("### ✨ 新增", body)
        for title in ("🐛 修复", "♻️ 重构", "📚 文档", "🔧 杂项", "📦 其他"):
            with self.subTest(title=title):
                self.assertNotIn(f"### {title}", body)

    def test_release_commit_is_dropped(self) -> None:
        commits = [
            rn.Commit(sha=SHA_A, subject="chore: release v0.7.0"),
            rn.Commit(sha=SHA_B, subject="feat: feature"),
        ]
        body = rn.render_notes(
            commits, version="v0.7.0", prev_tag=None, date="2024-05-06"
        )
        self.assertNotIn("chore: release v0.7.0", body)
        self.assertNotIn(SHA_A[:7], body)
        self.assertIn("共 1 个提交。", body)

    def test_first_release_hint(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.1.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn(rn.FIRST_RELEASE_HINT, body)
        self.assertNotIn("完整变更日志", body)
        self.assertNotIn("---", body)

    def test_empty_commit_set_hint(self) -> None:
        body = rn.render_notes([], version="v0.7.0", prev_tag=None, date="2024-05-06")
        self.assertIn(rn.EMPTY_HINT, body)
        self.assertNotIn("个提交。", body)
        self.assertNotIn("###", body)

    def test_only_release_commit_counts_as_empty(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="chore: release v0.7.0")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn(rn.EMPTY_HINT, body)

    def test_commit_ref_is_link_with_repo(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
            repo="owner/name",
        )
        self.assertIn(
            f"[`{SHA_A[:7]}`](https://github.com/owner/name/commit/{SHA_A})", body
        )

    def test_commit_ref_is_code_span_without_repo(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn(f"(`{SHA_A[:7]}`)", body)
        self.assertNotIn("https://github.com", body)

    def test_compare_link_with_prev_tag(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag="v0.6.0",
            date="2024-05-06",
            repo="owner/name",
        )
        self.assertIn(
            "**完整变更日志**: [`v0.6.0...v0.7.0`]"
            "(https://github.com/owner/name/compare/v0.6.0...v0.7.0)",
            body,
        )

    def test_compare_link_without_repo_is_plain_code_span(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag="v0.6.0",
            date="2024-05-06",
        )
        self.assertIn("**完整变更日志**: `v0.6.0...v0.7.0`", body)

    def test_no_compare_link_without_prev_tag(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertNotIn("完整变更日志", body)

    def test_markdown_special_chars_are_escaped(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: add `code` [link] <b>")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("add \\`code\\`", body)
        self.assertIn("\\[link\\]", body)
        self.assertIn("\\<b\\>", body)
        # 未转义的原文不应出现在 body 里。
        self.assertNotIn("[link]", body)
        self.assertNotIn("<b>", body)

    def test_non_conventional_commit_goes_to_other_group(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="Merge pull request #12")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("### 📦 其他", body)
        self.assertIn("Merge pull request #12", body)

    def test_breaking_and_scope_are_rendered_in_head(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat(api)!: drop v1")],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("- feat(api)!: drop v1", body)

    def test_breaking_footer_marks_head_like_bang(self) -> None:
        """body-only 的 `BREAKING CHANGE:` footer 与 `!` 等效，渲染时补 `!`。"""
        body = rn.render_notes(
            [
                rn.Commit(
                    sha=SHA_A,
                    subject="feat(api): drop v1",
                    body="legacy clients break\n\nBREAKING CHANGE: drop v1",
                )
            ],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("- feat(api)!: drop v1", body)

    def test_body_without_footer_is_not_marked_breaking(self) -> None:
        body = rn.render_notes(
            [
                rn.Commit(
                    sha=SHA_A,
                    subject="feat(api): add v2",
                    body="Refs: #12",
                )
            ],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("- feat(api): add v2", body)
        self.assertNotIn("feat(api)!:", body)

    def test_breaking_footer_and_bang_are_deduplicated(self) -> None:
        body = rn.render_notes(
            [
                rn.Commit(
                    sha=SHA_A,
                    subject="feat(api)!: drop v1",
                    body="BREAKING CHANGE: drop v1\n\nBREAKING-CHANGE: drop v1",
                )
            ],
            version="v0.7.0",
            prev_tag=None,
            date="2024-05-06",
        )
        self.assertIn("- feat(api)!: drop v1", body)
        self.assertNotIn("feat(api)!!", body)

    def test_entries_keep_git_log_order_within_group(self) -> None:
        commits = [
            rn.Commit(sha=SHA_A, subject="feat: newer"),
            rn.Commit(sha=SHA_B, subject="feat: older"),
        ]
        body = rn.render_notes(
            commits, version="v0.7.0", prev_tag=None, date="2024-05-06"
        )
        self.assertLess(body.index("feat: newer"), body.index("feat: older"))

    def test_single_trailing_newline(self) -> None:
        body = rn.render_notes(
            [rn.Commit(sha=SHA_A, subject="feat: feature")],
            version="v0.7.0",
            prev_tag="v0.6.0",
            date="2024-05-06",
        )
        self.assertTrue(body.endswith("\n"))
        self.assertFalse(body.endswith("\n\n"))


if __name__ == "__main__":
    unittest.main()
