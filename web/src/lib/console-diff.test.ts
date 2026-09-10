import { describe, expect, it } from "vitest";
import { checksSummary, diffFiles, diffStat, type DiffLine } from "@/lib/console-diff";

const PATCH = `diff --git a/crates/x.rs b/crates/x.rs
index 1111111..2222222 100644
--- a/crates/x.rs
+++ b/crates/x.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
diff --git a/web/y.ts b/web/y.ts
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/web/y.ts
@@ -0,0 +1,2 @@
+export const a = 1;
+export const b = 2;
`;

const kinds = (lines: readonly DiffLine[]) => lines.map((l) => l.kind);
const texts = (lines: readonly DiffLine[]) => lines.map((l) => l.text);

describe("diffFiles", () => {
  it("splits a patch at its `diff --git` boundaries and names each file", () => {
    const files = diffFiles(PATCH);
    expect(files.map((f) => f.path)).toEqual(["crates/x.rs", "web/y.ts"]);
  });

  // The `b/` side is the file as it exists AFTER the change, which is the name an operator is
  // looking for. `a/` and `b/` differ on a rename, and the post-image is the useful one.
  it("names a renamed file by its new path", () => {
    const files = diffFiles("diff --git a/old/name.ts b/new/name.ts\n@@ -1 +1 @@\n-a\n+b\n");
    expect(files[0].path).toBe("new/name.ts");
  });

  // A path with a space in it breaks a naive `split(" ")`, and git emits them unquoted here.
  it("reads a path containing a space", () => {
    const files = diffFiles('diff --git a/a dir/f.ts b/a dir/f.ts\n@@ -1 +1 @@\n-a\n+b\n');
    expect(files[0].path).toBe("a dir/f.ts");
  });

  it("classifies every line so the view colorizes without re-parsing", () => {
    const [first] = diffFiles(PATCH);
    expect(kinds(first.lines)).toEqual([
      "meta", // index …
      "meta", // --- a/…
      "meta", // +++ b/…
      "hunk", // @@ …
      "context",
      "del",
      "add",
      "context",
    ]);
    expect(texts(first.lines).slice(4)).toEqual([" fn main() {", "-    old();", "+    new();", " }"]);
  });

  // `--- a/x` and `+++ b/x` begin with `-` and `+` and are NOT a deletion and an addition. Reading
  // them as such is the classic unified-diff bug: every file would show two phantom changed lines,
  // and the +/- tallies beside it would be wrong on every file in the patch.
  it("does not mistake the ---/+++ file headers for a deletion and an addition", () => {
    const [first] = diffFiles(PATCH);
    const header = first.lines.filter((l) => l.text.startsWith("---") || l.text.startsWith("+++"));
    expect(header).toHaveLength(2);
    expect(header.every((l) => l.kind === "meta")).toBe(true);
  });

  // `/dev/null` on the a-side is how git spells a new file, and the b-side path is still the name.
  it("names a new file from its b-side path", () => {
    const files = diffFiles(PATCH);
    expect(files[1].path).toBe("web/y.ts");
    expect(kinds(files[1].lines).filter((k) => k === "add")).toHaveLength(2);
  });

  it("has no files for an empty or whitespace-only patch", () => {
    for (const patch of ["", "\n", "   \n\n"]) {
      expect(diffFiles(patch)).toEqual([]);
    }
  });

  // A truncated patch ends mid-file, and its last file must still render rather than being dropped
  // for lacking a following `diff --git`.
  it("keeps the trailing file of a patch that was cut short", () => {
    const cut = "diff --git a/x.rs b/x.rs\n@@ -1,9 +1,9 @@\n-a\n+b\n-c";
    const files = diffFiles(cut);
    expect(files).toHaveLength(1);
    expect(files[0].path).toBe("x.rs");
    expect(kinds(files[0].lines)).toEqual(["hunk", "del", "add", "del"]);
  });

  // A patch `gh` produced without a `diff --git` preamble (or one whose head was cut off) still
  // has lines worth showing. Dropping them would render an empty panel for a non-empty diff.
  it("shows a patch with no file header under no path rather than showing nothing", () => {
    const files = diffFiles("@@ -1 +1 @@\n-a\n+b\n");
    expect(files).toHaveLength(1);
    expect(files[0].path).toBe("");
    expect(kinds(files[0].lines)).toEqual(["hunk", "del", "add"]);
  });

  // The other half of the ---/+++ rule, and the one a prefix test gets wrong. A deleted `--i;`
  // renders as `---i;` and a deleted `--` as `---`; INSIDE a hunk those are deletions, and greying
  // them out as metadata would silently drop real removed lines from any C-like or YAML file — and
  // from the +/- tallies beside them.
  it("reads ---/+++ inside a hunk as a deletion and an addition, not as headers", () => {
    const patch = [
      "diff --git a/x.c b/x.c",
      "--- a/x.c",
      "+++ b/x.c",
      "@@ -1,4 +1,4 @@",
      "---i;",
      "---",
      "+++i;",
      "+++",
      "",
    ].join("\n");
    const [file] = diffFiles(patch);
    expect(kinds(file.lines)).toEqual(["meta", "meta", "hunk", "del", "del", "add", "add"]);
    expect(diffStat([file])).toEqual({ files: 1, added: 2, removed: 2 });
  });

  // And the flag is per FILE: the second file's own header must still be read as one, even though
  // the first file already had a hunk.
  it("reads the second file's headers as headers again", () => {
    const [, second] = diffFiles(PATCH);
    // Its `--- /dev/null` and `+++ b/web/y.ts` are still METADATA, even though the FIRST file in
    // this patch already passed a `@@` — the flag resets at every `diff --git`.
    const headers = second.lines.filter(
      (l) => l.text.startsWith("---") || l.text.startsWith("+++"),
    );
    expect(headers).toHaveLength(2);
    expect(headers.every((l) => l.kind === "meta")).toBe(true);
    // And the additions after its hunk are still additions.
    expect(kinds(second.lines).filter((k) => k === "add")).toHaveLength(2);
  });

  it("keeps a blank context line, which is a real unchanged empty line", () => {
    const files = diffFiles("diff --git a/x b/x\n@@ -1,2 +1,2 @@\n\n+a\n");
    expect(kinds(files[0].lines)).toEqual(["hunk", "context", "add"]);
  });
});

describe("diffStat", () => {
  it("counts added and removed lines, and neither file header", () => {
    expect(diffStat(diffFiles(PATCH))).toEqual({ files: 2, added: 3, removed: 1 });
  });

  it("is all zeroes for no files", () => {
    expect(diffStat([])).toEqual({ files: 0, added: 0, removed: 0 });
  });
});

describe("checksSummary", () => {
  // A one-line reading, because the panel has room for one. FAILURE is the state worth naming
  // outright; anything still running is the next most useful thing to say.
  it("names a failure over anything else", () => {
    expect(
      checksSummary([
        { name: "lint", state: "SUCCESS" },
        { name: "test", state: "FAILURE" },
        { name: "web", state: "IN_PROGRESS" },
      ]),
    ).toBe("1 of 3 checks failing");
  });

  it("says what is still running when nothing has failed", () => {
    expect(
      checksSummary([
        { name: "lint", state: "SUCCESS" },
        { name: "test", state: "IN_PROGRESS" },
      ]),
    ).toBe("1 of 2 checks still running");
  });

  it("says all passing only when they all did", () => {
    expect(
      checksSummary([
        { name: "lint", state: "SUCCESS" },
        { name: "test", state: "SUCCESS" },
      ]),
    ).toBe("2 checks passing");
  });

  // Empty is not "0 checks passing": GitHub answers an empty rollup both for a pull request with
  // no checks configured AND when the daemon could not read them, and neither is a verdict.
  it("says nothing at all when there are no checks", () => {
    expect(checksSummary([])).toBe("");
  });

  // GitHub's vocabulary is its own and has grown before. A state this console has never heard of
  // must not be silently counted as a pass — that is the one wrong answer available here.
  it("does not count an unrecognised state as passing", () => {
    expect(
      checksSummary([
        { name: "lint", state: "SUCCESS" },
        { name: "test", state: "SOMETHING_NEW" },
      ]),
    ).toBe("1 of 2 checks passing");
  });

  it("reads a cancelled or timed-out check as a failure, since neither is a pass", () => {
    for (const state of ["CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE"]) {
      expect(checksSummary([{ name: "test", state }])).toBe("1 of 1 checks failing");
    }
  });

  // `COMPLETED` is a check RUN's status, not its conclusion — a completed check that FAILED has
  // status COMPLETED — so counting it as a pass would be the exact wrong-direction guess this
  // closed list exists to refuse.
  it("does not count a bare COMPLETED status as a pass", () => {
    expect(checksSummary([{ name: "test", state: "COMPLETED" }])).toBe("0 of 1 checks passing");
  });

  it("reads a neutral or skipped check as neither passing nor failing", () => {
    expect(
      checksSummary([
        { name: "lint", state: "SUCCESS" },
        { name: "optional", state: "SKIPPED" },
      ]),
    ).toBe("1 of 2 checks passing");
  });
});
