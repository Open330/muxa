import assert from "node:assert/strict";
import test from "node:test";

import { landingAccess, pickLanguage } from "../src/dashboard/web/landing.mjs";

test("landing language: saved choice, then browser preference, then English", () => {
  assert.equal(pickLanguage("ko", ["en-US"]), "ko");
  assert.equal(pickLanguage(null, ["ko-KR", "en"]), "ko");
  assert.equal(pickLanguage("fr", ["de-DE", "en-GB"]), "en");
  assert.equal(pickLanguage(null, []), "en");
});

test("landing offers sign-in first when the server has a login provider", () => {
  const access = landingAccess({ available: true, signedIn: false, role: "none" });
  assert.equal(access.signIn, true);
  assert.equal(access.token, true);
  assert.equal(access.tokenPrimary, false);
  assert.equal(access.signOut, false);
});

test("landing falls back to the token when there is no login provider", () => {
  const access = landingAccess({ available: false, signedIn: false, role: "none" });
  assert.equal(access.signIn, false);
  assert.equal(access.tokenPrimary, true);
});

test("a signed-in account without access is told so and can sign out", () => {
  const access = landingAccess({ available: true, signedIn: true, role: "none", email: "a@b.c" });
  assert.equal(access.signIn, false);
  assert.equal(access.signOut, true);
  assert.equal(access.signedInNoAccess, true);
});

test("a rejected stored token is reported", () => {
  assert.equal(landingAccess(null, { tokenRejected: true }).tokenRejected, true);
  assert.equal(landingAccess(null).tokenRejected, false);
});
