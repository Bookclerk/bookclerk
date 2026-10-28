// Shared by vitest.config.ts (which binds the databases) and d1.ts (which
// hands them out); kept apart because d1.ts imports the Workers runtime.

/** How many D1 databases one test can create. */
export const TEST_DB_POOL_SIZE = 16;
