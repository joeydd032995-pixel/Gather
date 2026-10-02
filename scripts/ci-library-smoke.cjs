// Black-box UI regression: real React app, API responses controlled for races.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn, execFileSync } = require("node:child_process");
const cli = fs.realpathSync(execFileSync("which", ["playwright"], { encoding: "utf8" }).trim());
const { chromium } = require(path.dirname(cli));
const root = path.resolve(__dirname, "..");
const vite = spawn("npm", ["--prefix", "apps/desktop", "run", "dev", "--",
    "--host", "127.0.0.1", "--port", "1420", "--strictPort"],
    { cwd: root, stdio: "inherit" });
const original = Array.from({ length: 200 }, (_, i) => ({
    id: "file-" + i, kind: "document_markdown", source_platform: "upload",
    original_filename: "file-" + i + ".md", media_type: "text/markdown",
    byte_size: 20, ingested_at: "2026-01-01T00:00:00Z", unit_count: 0, status: "done",
}));
const uploaded = [];
let held = null;
let holdLimit = 0;
let browser;

async function waitFor(check, label, timeout = 20000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
        const result = await check();
        if (result) return result;
        await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw new Error("Timed out: " + label);
}
function items() {
    return [...uploaded].reverse().map((name, i) => ({
        ...original[0], id: "fresh-" + (uploaded.length - i), original_filename: name,
    })).concat(original);
}
const cors = {
    "access-control-allow-origin": "*",
    "access-control-allow-methods": "GET,POST,OPTIONS",
    "access-control-allow-headers": "Content-Type,Authorization",
};

(async () => {
    try {
        await waitFor(async () => {
            try { return (await fetch("http://127.0.0.1:1420")).ok; } catch { return false; }
        }, "Vite startup");
        browser = await chromium.launch({ headless: true });
        const page = await browser.newPage();
        const errors = [];
        const remote = [];
        page.on("pageerror", error => errors.push(error.message));
        page.on("request", request => {
            if (!request.url().startsWith("http://127.0.0.1:")) remote.push(request.url());
        });
        await page.route("http://127.0.0.1:7601/**", async route => {
            const request = route.request();
            const url = new URL(request.url());
            const send = value => route.fulfill({
                status: 200, contentType: "application/json", headers: cors,
                body: JSON.stringify(value),
            });
            if (request.method() === "OPTIONS") return route.fulfill({ status: 204, headers: cors });
            if (url.pathname === "/healthz" || url.pathname === "/readyz") return send({});
            if (url.pathname === "/api/v1/artifacts") {
                const limit = Number(url.searchParams.get("limit"));
                if (limit === holdLimit && !held) {
                    held = route;
                    return;
                }
                return send({ items: items().slice(0, limit) });
            }
            if (url.pathname === "/api/v1/ingest/files") {
                const name = request.postData().match(/filename="([^"]+)"/)[1];
                uploaded.push(name);
                return send({ job_id: "upload-" + uploaded.length, files: [{
                    filename: name, kind: "document_markdown", artifact_id: "fresh-" + uploaded.length,
                    status: "accepted", deduplicated: false, detail: null, segments: 1,
                }] });
            }
            const artifact = url.pathname.match(/^\/api\/v1\/artifacts\/([^/]+)(\/content)?$/);
            if (artifact) {
                if (artifact[2]) return send({
                    source: "document", total: 1,
                    items: [{ seq: 0, heading: null, page: null, role: null,
                              text: uploaded.length ? "Refreshed selected detail" : "Original detail" }],
                });
                return send({ ...original.find(f => f.id === artifact[1]),
                    document: { page_count: null, segment_count: 1, extraction_status: "done" },
                    image: null, conversations: [],
                });
            }
            if (url.pathname === "/api/v1/graph") return send({
                entities: [], files: [], relations: [], mentions: [], projects: [], folders: [],
                contains: [], similar: [], entity_total: 0, truncated: false,
            });
            // Other views/counts are not part of this deterministic UI fixture.
            return send({ items: [], hits: [], total: 0 });
        });
        await page.goto("http://127.0.0.1:1420");
        await page.getByRole("navigation", { name: "Main" })
            .getByRole("button", { name: "Library", exact: true }).click();
        await waitFor(async () => (await page.locator("[data-file]").count()) === 50, "first page");
        await page.locator("[data-file]").filter({ hasText: "file-0.md" }).click();
        await page.getByText("Original detail", { exact: true }).waitFor();
        for (const [depth, staleError] of [[100, false], [150, true]]) {
            held = null;
            holdLimit = depth + 1;
            await page.getByRole("button", { name: "Show more files", exact: true }).click();
            const old = await waitFor(() => held, "pending older pagination request");
            holdLimit = 0;
            const name = "fresh-" + (uploaded.length + 1) + ".md";
            await page.locator('input[type="file"]').first().setInputFiles({
                name, mimeType: "text/markdown", buffer: Buffer.from("# Local upload"),
            });
            await page.locator("[data-file]").filter({ hasText: name }).waitFor();
            await waitFor(async () => (await page.locator("[data-file]").count()) === depth,
                          "pagination depth retained after upload");
            assert.equal(await page.locator('[data-file][aria-current="true"] .row-title')
                .textContent(), "file-0.md");
            await page.getByText("Refreshed selected detail", { exact: true }).waitFor();
            const response = page.waitForResponse(res => res.request() === old.request());
            await old.fulfill({
                status: staleError ? 500 : 200, contentType: "application/json", headers: cors,
                body: JSON.stringify(staleError
                    ? { error: { message: "stale request failed" } }
                    : { items: original.slice(0, depth + 1) }),
            });
            await (await response).finished();
            await page.evaluate(() => new Promise(resolve => {
                requestAnimationFrame(() => requestAnimationFrame(resolve));
            }));
            assert.equal(await page.locator("[data-file]").count(), depth);
            assert.equal(await page.locator("[data-file]").filter({ hasText: name }).count(), 1);
            assert.equal(await page.getByText("stale request failed", { exact: true }).count(), 0);
            assert.equal(await page.locator('[data-file][aria-current="true"] .row-title')
                .textContent(), "file-0.md");
            console.log("PASS: upload refresh preserves selection/depth and rejects stale " +
                        (staleError ? "errors" : "successes"));
        }
        assert.deepEqual(errors, [], "Frontend runtime errors");
        assert.deepEqual(remote, [], "Frontend requested a non-loopback resource");
        console.log("PASS: Library refresh/detail and offline frontend regression");
    } finally {
        if (browser) await browser.close();
        vite.kill("SIGTERM");
    }
})().catch(error => { console.error(error); process.exitCode = 1; });
