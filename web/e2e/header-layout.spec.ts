import { expect, test, type Locator, type Page } from "@playwright/test";

// The width acceptance for the three-row run-detail header (STUDIO-1023). The unit suite holds the
// structure; this is the one place a real engine measures it. Three fixtures — a one-attempt
// ticket, the 11-attempt/25-review STUDIO-988 shape, and a review run — at 400, 1440, 1728 and
// 1920px. Run with `npm run test:layout` (needs `npx playwright install chromium`).

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

      // The title is shown in full, never end-ellipsized.
      const h1 = header.locator(".idw h1");
      const title = await h1.textContent();
      expect(title ?? "").not.toContain("…");
      expect(title?.trim().length ?? 0).toBeGreaterThan(0);
      // At desktop width it fits within the two balanced lines.
      if (width >= 1440) {
        const { clientHeight, lineHeight } = await h1.evaluate((el) => ({
          clientHeight: el.clientHeight,
          lineHeight: parseFloat(getComputedStyle(el).lineHeight) || 18,
        }));
        expect(clientHeight, "title clipped at desktop width").toBeLessThanOrEqual(
          lineHeight * 2 + 2,
        );
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
      // never a `jim…` fragment.
      const attemptLabels = await header.locator(".trattempts button, .trattempts option").allTextContents();
      for (const label of attemptLabels) expect(label).not.toContain("…");
      if (fixture === "many") {
        const select = header.locator("select.trattempts");
        await expect(select).toHaveCount(1);
        const selected = await select.evaluate(
          (el: HTMLSelectElement) => el.options[el.selectedIndex]?.textContent ?? "",
        );
        expect(selected).toBe("attempt 11 of 11 · alice");
      } else {
        expect(await header.locator("select.trattempts").count()).toBe(0);
      }

      // The branch is full or middle-ellipsized, and the FULL value is always in the tooltip.
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

      // Actions stay right-aligned within the controls row.
      const acts = await requireBox(header.locator(".acts"));
      expect(Math.abs(acts.x + acts.width - (ctl.x + ctl.width))).toBeLessThanOrEqual(3);

      // --- Review-run correctness, at every width -----------------------------------------
      if (fixture === "review") {
        // No Merge at all on a `pr:` run — not disabled, absent.
        await expect(header.locator(".acts").getByRole("button", { name: /^merge$/i })).toHaveCount(0);
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
