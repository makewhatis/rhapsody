import { expect, test, type Locator, type Page } from "@playwright/test";

// The width acceptance for the three-row run-detail header (STUDIO-1023). The unit suite holds the
// structure; this is the one place a real engine measures it. Three fixtures — a one-attempt
// ticket, the 11-attempt/25-review STUDIO-988 shape, and a review run — at 400, 1440, 1728 and
// 1920px. Run with `npm run test:layout` (needs `npx playwright install chromium`).
//
// Every "not cut" assertion here measures what is DRAWN, not `textContent`: a CSS
// `text-overflow: ellipsis` never changes `textContent`, which is how `Review makew…` and
// `attempt 5 · jim…` shipped green (STUDIO-1023 round 2). `hClipped` walks the element and its
// descendants and reports any box whose painted content is wider than its own client box.

const WIDTHS = [400, 1440, 1728, 1920];
const FIXTURES = ["one", "many", "review"] as const;

interface FixtureState {
  name: string;
  provenance: { label: string; value: string; origin: string; override: boolean }[];
  branch: string;
}

async function load(page: Page, fixture: string, width: number): Promise<FixtureState> {
  await page.setViewportSize({ width, height: 1000 });
  await page.goto(`/layout.html?fixture=${fixture}`);
  await page.waitForSelector(".trhd .idw h1");
  await page.evaluate(() => document.fonts.ready);
  return (await page.evaluate(() => window.__layoutFixture)) as FixtureState;
}

/** Two boxes overlap if they share area on BOTH axes. */
function overlaps(
  a: { x: number; y: number; width: number; height: number },
  b: { x: number; y: number; width: number; height: number },
): boolean {
  return a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
}

async function requireBox(loc: Locator) {
  const box = await loc.boundingBox();
  expect(box, `no box for ${String(loc)}`).not.toBeNull();
  return box as NonNullable<typeof box>;
}

/**
 * The rect the element's TEXT actually paints into. A `Range` over the contents reports the drawn
 * extent, which is what must not reach under the assignee — an element BOX reports its own
 * (clipped) bounds and hid the overlapping key.
 */
async function textRect(loc: Locator) {
  return loc.evaluate((el) => {
    const range = document.createRange();
    range.selectNodeContents(el);
    const r = range.getBoundingClientRect();
    return { x: r.x, y: r.y, width: r.width, height: r.height };
  });
}

/**
 * Every element at or under `loc` whose painted content is wider than its client box — a CSS
 * ellipsis, a `nowrap` overflow, the reported `clim…` cut. Returns a description per offender.
 */
async function hClipped(loc: Locator): Promise<string[]> {
  return loc.evaluate((root) => {
    const bad: string[] = [];
    const walk = (el: HTMLElement) => {
      if (el.clientWidth > 0 && el.scrollWidth > el.clientWidth + 1) {
        bad.push(`${el.className || el.tagName} ${el.scrollWidth}>${el.clientWidth}`);
      }
      for (const child of Array.from(el.children)) {
        if (child instanceof HTMLElement) walk(child);
      }
    };
    walk(root as HTMLElement);
    return bad;
  });
}

for (const fixture of FIXTURES) {
  for (const width of WIDTHS) {
    test(`${fixture} header lays out cleanly at ${width}px`, async ({ page }) => {
      const state = await load(page, fixture, width);
      const header = page.locator(".trhd");

      // The page body never scrolls sideways; the provenance row scrolls inside its own box.
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth),
        "page scrolls sideways",
      ).toBeLessThanOrEqual(width + 1);

      // --- Row 1: identity -----------------------------------------------------------------
      // The provenance is NOT under the title — the exact overlap the report showed.
      expect(await header.locator(".idw .prov").count()).toBe(0);
      expect(await header.locator(".trprovrow .prov").count()).toBe(1);

      const idw = await requireBox(header.locator(".idw"));
      const who = await requireBox(header.locator(".who2"));
      const pill = await requireBox(header.locator(".trhd-id > .pill"));
      // The avatar/name never sits on top of the title, and it is to the RIGHT of it.
      expect(overlaps(idw, who), "assignee overlaps the title").toBe(false);
      expect(overlaps(idw, pill), "outcome pill overlaps the title").toBe(false);
      expect(idw.x + idw.width).toBeLessThanOrEqual(who.x + 1);

      // The run key (`pr:owner/repo#N@reviewer` has no break point of its own) is drawn inside the
      // identity column, never over the assignee — measured against the TEXT, not the box.
      const keyEl = header.locator(".idw .k");
      const keyText = await textRect(keyEl);
      expect(overlaps(keyText, who), "run key drawn under the assignee").toBe(false);
      expect(keyText.x + keyText.width, "run key overflows its column").toBeLessThanOrEqual(
        idw.x + idw.width + 1,
      );
      expect(await hClipped(keyEl), "run key cut sideways").toEqual([]);

      // The title is shown in full, never end-ellipsized, and never drawn under the assignee.
      const h1 = header.locator(".idw h1");
      const titleText = await textRect(h1);
      expect(overlaps(titleText, who), "title drawn under the assignee").toBe(false);
      const title = await h1.textContent();
      expect(title ?? "").not.toContain("…");
      expect(title?.trim().length ?? 0).toBeGreaterThan(0);
      // No horizontal cut at ANY width — a CSS ellipsis shows here as `scrollWidth > clientWidth`.
      expect(await hClipped(h1), "title cut sideways").toEqual([]);
      // At desktop width it fits within the two balanced lines (the line-clamp is vertical; at
      // 400px a long title is allowed to clamp to two lines, which the ticket's "at most two lines"
      // permits — the horizontal cut above is what is forbidden everywhere).
      if (width >= 1440) {
        const { clientHeight, scrollHeight, lineHeight } = await h1.evaluate((el) => ({
          clientHeight: el.clientHeight,
          scrollHeight: el.scrollHeight,
          lineHeight: parseFloat(getComputedStyle(el).lineHeight) || 18,
        }));
        expect(clientHeight, "title clipped at desktop width").toBeLessThanOrEqual(
          lineHeight * 2 + 2,
        );
        expect(scrollHeight, "title clamped at desktop width").toBeLessThanOrEqual(clientHeight + 1);
      }

      // --- Row 2: provenance ---------------------------------------------------------------
      const provrow = await requireBox(header.locator(".trprovrow"));
      expect(provrow.y, "provenance overlaps the identity row").toBeGreaterThanOrEqual(
        idw.y + idw.height - 1,
      );
      const chips = header.locator(".trprovrow .pf");
      expect(await chips.count()).toBe(3);
      for (let i = 0; i < 3; i += 1) {
        const chip = chips.nth(i);
        const box = await requireBox(chip);
        // A value that wrapped one fragment per line would make the chip tall; nowrap keeps it
        // one line.
        expect(box.height, `provenance chip ${i} wrapped`).toBeLessThanOrEqual(30);
        expect(await chip.locator(".pv").textContent()).toBe(state.provenance[i].value);
        const ws = await chip.locator(".pv").evaluate((el) => getComputedStyle(el).whiteSpace);
        expect(ws).toBe("nowrap");
      }

      // --- Row 3: controls -----------------------------------------------------------------
      const ctl = await requireBox(header.locator(".trctl"));
      expect(ctl.y, "controls overlap the provenance row").toBeGreaterThanOrEqual(
        provrow.y + provrow.height - 1,
      );
      expect(
        await header.locator(".trctl").evaluate((el) => el.scrollWidth),
        "controls row overflows sideways",
      ).toBeLessThanOrEqual(await header.locator(".trctl").evaluate((el) => el.clientWidth) + 1);

      // Attempts: one full label, or a compact dropdown that still names the teammate in full —
      // never a `jim…` fragment. The segment labels are measured as DRAWN.
      const attemptLabels = await header.locator(".trattempts button, .trattempts option").allTextContents();
      for (const label of attemptLabels) expect(label).not.toContain("…");
      const segments = header.locator(".trattempts button");
      for (let i = 0; i < (await segments.count()); i += 1) {
        expect(await hClipped(segments.nth(i)), `attempt label ${i} cut`).toEqual([]);
      }
      if (fixture === "many") {
        const select = header.locator("select.trattempts");
        await expect(select).toHaveCount(1);
        const selected = await select.evaluate(
          (el: HTMLSelectElement) => el.options[el.selectedIndex]?.textContent ?? "",
        );
        expect(selected).toBe("attempt 11 of 11 · alice");
        expect(await hClipped(select), "attempt dropdown cut").toEqual([]);
      } else {
        expect(await header.locator("select.trattempts").count()).toBe(0);
      }

      // The branch is full or middle-ellipsized, and the FULL value is always in the tooltip. The
      // drawn label must fit its own box — a CSS end-ellipsis would show as `scrollWidth` overrun.
      const mono = header.locator(".trbranch .mono");
      await expect(mono).toHaveAttribute("title", state.branch);
      const shown = (await mono.textContent()) ?? "";
      expect(shown).not.toMatch(/jim…$/);
      if (shown.includes("…")) {
        expect(shown.length).toBeLessThanOrEqual(34);
        // Both ends survive the middle ellipsis.
        const [head, tail] = shown.split("…");
        expect(state.branch.startsWith(head)).toBe(true);
        expect(state.branch.endsWith(tail)).toBe(true);
      } else {
        expect(shown).toBe(state.branch);
      }
      expect(await hClipped(mono), "branch cut sideways").toEqual([]);

      // Actions stay right-aligned within the controls row.
      const acts = await requireBox(header.locator(".acts"));
      expect(Math.abs(acts.x + acts.width - (ctl.x + ctl.width))).toBeLessThanOrEqual(3);

      // --- Review-run correctness, at every width -----------------------------------------
      if (fixture === "review") {
        // No Merge on a `pr:` run — not disabled, absent. `getByRole("button", { name: /^merge$/i })`
        // misses a dependency-named Merge (whose name is "Merge dep"), so the WORD is asserted to be
        // absent from the cluster: MUTATION GUARD, drop the render guard and a disabled "Asking the
        // daemon…" Merge appears here.
        expect(await header.locator(".acts").textContent()).not.toMatch(/merge/i);
        await expect(header.locator(".acts").getByRole("link", { name: /open origin ticket/i })).toHaveAttribute(
          "href",
          /\/issue\/STUDIO-988$/,
        );
        await expect(header.locator(".acts").getByRole("link", { name: /view pr/i })).toHaveAttribute(
          "href",
          /\/pull\/223$/,
        );
      }
    });
  }
}

test("the 11-attempt / 25-review strip stays bounded, not two rows of 25 chips", async ({
  page,
}) => {
  await load(page, "many", 1440);
  await expect(page.locator(".trrev")).toHaveCount(5);
  await expect(page.locator(".trrevmore")).toHaveText("+4 earlier rounds");
});
