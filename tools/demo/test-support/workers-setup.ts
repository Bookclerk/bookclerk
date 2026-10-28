// Runs before every test in the `workers` project (vitest.config.ts): drops
// what the previous test stored, so the databases d1.ts hands out start
// empty.

import { reset } from "cloudflare:test";
import { beforeEach } from "vitest";

import { resetHandOut } from "./d1.ts";

beforeEach(async () => {
  await reset();
  resetHandOut();
});
