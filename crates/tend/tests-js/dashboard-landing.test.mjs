import assert from "node:assert/strict";
import test from "node:test";

import { landingAccess, pickLanguage } from "../src/dashboard/web/landing.mjs";

test("landing language: saved choice, then browser preference, then English", () => {
  assert.equal(pickLanguage("ko", ["en-US"]), "ko");
  assert.equal(pickLanguage(null, ["ko-KR", "en"]), "ko");
  assert.equal(pickLanguage("fr", ["de-DE", "en-GB"]), "en");
  assert.equal(pickLanguage(null, []), "en");
});

test("the landing points at the tend init link, not a token field or a sign-in", () => {
  const access = landingAccess();
  assert.equal(access.linkHint, true);
  assert.equal("token" in access, false);
  assert.equal("signIn" in access, false);
});

test("a rejected stored token is reported", () => {
  assert.equal(landingAccess({ tokenRejected: true }).tokenRejected, true);
  assert.equal(landingAccess().tokenRejected, false);
});
