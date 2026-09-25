import test from "node:test";
import assert from "node:assert/strict";
import enCommon from "../src/shared/i18n/locales/en/common";
import zhCommon from "../src/shared/i18n/locales/zh-CN/common";
import enHome from "../src/shared/i18n/locales/en/home";
import zhHome from "../src/shared/i18n/locales/zh-CN/home";

function tokens(value: string): string[] {
  return [...value.matchAll(/{{\s*([^}]+?)\s*}}|<([a-z]+)>/g)]
    .map((match) => match[0])
    .sort();
}
function compare(reference: unknown, translated: unknown, path: string) {
  if (typeof reference === "string") {
    assert.equal(typeof translated, "string", path);
    assert.ok((translated as string).trim(), path);
    assert.deepEqual(tokens(translated as string), tokens(reference), path);
    return;
  }
  assert.ok(reference && typeof reference === "object", path);
  assert.ok(translated && typeof translated === "object", path);
  assert.deepEqual(
    Object.keys(translated).sort(),
    Object.keys(reference).sort(),
    path,
  );
  for (const key of Object.keys(reference)) {
    compare(
      (reference as Record<string, unknown>)[key],
      (translated as Record<string, unknown>)[key],
      path + "." + key,
    );
  }
}

test("locale catalogs have matching keys, interpolation variables and rich text tags", () => {
  compare(enCommon, zhCommon, "common");
  compare(enHome, zhHome, "home");
});
