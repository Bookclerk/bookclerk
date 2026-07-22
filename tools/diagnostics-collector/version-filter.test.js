import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  compareSemver,
  extractVersionFromKey,
  isPackagingDerivative,
  normalizeVersion,
  versionAcceptable,
} from "./version-filter.js";

describe("normalizeVersion", () => {
  it("strips leading v", () => {
    assert.equal(normalizeVersion("v1.2.3"), "1.2.3");
    assert.equal(normalizeVersion("V0.1.0"), "0.1.0");
  });
});

describe("extractVersionFromKey", () => {
  it("reads diagnostics/<version>/<id>.json", () => {
    assert.equal(
      extractVersionFromKey("diagnostics/0.1.0/550e8400-e29b-41d4-a716-446655440000.json"),
      "0.1.0",
    );
    assert.equal(extractVersionFromKey("diagnostics/unknown/x.json"), null);
    assert.equal(extractVersionFromKey("other/0.1.0/x.json"), null);
  });
});

describe("versionAcceptable", () => {
  it("accepts all when baseline is empty (no releases yet)", () => {
    assert.equal(versionAcceptable("0.0.1", null), true);
    assert.equal(versionAcceptable("0.0.1", ""), true);
    assert.equal(versionAcceptable(null, null), true);
  });

  it("keeps equal, newer, and packaging derivatives", () => {
    const baseline = "1.2.3";
    assert.equal(versionAcceptable("1.2.3", baseline), true);
    assert.equal(versionAcceptable("v1.2.3", baseline), true);
    assert.equal(versionAcceptable("1.2.4", baseline), true);
    assert.equal(versionAcceptable("2.0.0-rc.1", baseline), true);
    assert.equal(versionAcceptable("1.2.3+nix", baseline), true);
    assert.equal(versionAcceptable("1.2.3-1", baseline), true);
    assert.equal(versionAcceptable("1.2.3-1ubuntu2", baseline), true);
    assert.equal(versionAcceptable("1.2.3.fc40", baseline), true);
  });

  it("rejects older and same-version prereleases", () => {
    const baseline = "1.2.3";
    assert.equal(versionAcceptable("1.2.2", baseline), false);
    assert.equal(versionAcceptable("1.2.3-rc.1", baseline), false);
    assert.equal(versionAcceptable("1.2.2+foo", baseline), false);
    assert.equal(versionAcceptable(null, baseline), false);
    assert.equal(versionAcceptable("unknown", baseline), false);
  });

  it("does not treat 1.2.30 as a packaging derivative of 1.2.3", () => {
    assert.equal(isPackagingDerivative("1.2.30", "1.2.3"), false);
    assert.equal(compareSemver("1.2.30", "1.2.3"), 1);
    assert.equal(versionAcceptable("1.2.30", "1.2.3"), true); // newer patch
  });
});
