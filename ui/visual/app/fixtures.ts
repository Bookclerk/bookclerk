/**
 * Sample API answers for the web app's page screenshots (app.spec.ts): one
 * small library (public-domain books on two storefronts, an owner, two
 * patrons and a branch) seen at a fixed time, so a screenshot changes only
 * when the app's layout does.
 *
 * Every JSON answer is checked against the daemon's OpenAPI document
 * (openapi.ts) before the app sees it.
 */

/** The fixed "now" every page is rendered at, ISO 8601 UTC. */
export const NOW = "2026-01-15T12:00:00Z";

/** NOW as Unix milliseconds. */
export const NOW_MS = Date.parse(NOW);

/**
 * An ISO timestamp some time before NOW.
 *
 * @param minutes - Minutes before NOW.
 * @returns The timestamp.
 */
function ago(minutes: number): string {
  return new Date(NOW_MS - minutes * 60_000).toISOString().replace(".000Z", "Z");
}

/** Minutes in a day. */
const DAY = 1440;

/** Who the page is rendered for. */
export type Viewer = "owner" | "operator" | "signed-out";

/** One answer to an API request. */
export type Answer = {
  /** HTTP status; default 200. */
  status?: number;
  /** JSON body, checked against the OpenAPI document. */
  json?: unknown;
  /** A non-JSON body (an image), with its content type. */
  body?: { contentType: string; text: string };
};

/** One book in the sample library. */
type Book = {
  /** Work id. */
  id: string;
  /** Edition id, also its cover's key. */
  edition: string;
  /** Title. */
  title: string;
  /** Authors, comma-separated. */
  authors: string;
  /** Narrators, comma-separated. */
  narrators: string;
  /** Series name, if any. */
  series?: string;
  /** Position in the series. */
  position?: string;
  /** Length in minutes. */
  minutes: number;
  /** Where the copy is. */
  status: "acquired" | "downloading" | "queued" | "not_acquired" | "error";
  /** The storefront it was bought on. */
  store: "echo" | "libro";
  /** Days before NOW it was bought. */
  boughtDaysAgo: number;
};

/** The sample library, in title order. */
const BOOKS: Book[] = [
  {
    id: "w-alice",
    edition: "e-alice",
    title: "Alice's Adventures in Wonderland",
    authors: "Lewis Carroll",
    narrators: "Ada Quill",
    minutes: 172,
    status: "acquired",
    store: "echo",
    boughtDaysAgo: 120,
  },
  {
    id: "w-dracula",
    edition: "e-dracula",
    title: "Dracula",
    authors: "Bram Stoker",
    narrators: "Theo Marsh, Ada Quill",
    minutes: 941,
    status: "acquired",
    store: "libro",
    boughtDaysAgo: 64,
  },
  {
    id: "w-frankenstein",
    edition: "e-frankenstein",
    title: "Frankenstein; or, The Modern Prometheus",
    authors: "Mary Shelley",
    narrators: "Iris Vale",
    minutes: 518,
    status: "downloading",
    store: "echo",
    boughtDaysAgo: 2,
  },
  {
    id: "w-moby",
    edition: "e-moby",
    title: "Moby-Dick",
    authors: "Herman Melville",
    narrators: "Theo Marsh",
    minutes: 1458,
    status: "acquired",
    store: "libro",
    boughtDaysAgo: 300,
  },
  {
    id: "w-pride",
    edition: "e-pride",
    title: "Pride and Prejudice",
    authors: "Jane Austen",
    narrators: "Iris Vale",
    minutes: 707,
    status: "acquired",
    store: "echo",
    boughtDaysAgo: 41,
  },
  {
    id: "w-sherlock",
    edition: "e-sherlock",
    title: "The Adventures of Sherlock Holmes",
    authors: "Arthur Conan Doyle",
    narrators: "Theo Marsh",
    series: "Sherlock Holmes",
    position: "3",
    minutes: 626,
    status: "queued",
    store: "libro",
    boughtDaysAgo: 1,
  },
  {
    id: "w-hound",
    edition: "e-hound",
    title: "The Hound of the Baskervilles",
    authors: "Arthur Conan Doyle",
    narrators: "Theo Marsh",
    series: "Sherlock Holmes",
    position: "5",
    minutes: 381,
    status: "acquired",
    store: "libro",
    boughtDaysAgo: 90,
  },
  {
    id: "w-time",
    edition: "e-time",
    title: "The Time Machine",
    authors: "H. G. Wells",
    narrators: "Ada Quill",
    minutes: 215,
    status: "error",
    store: "echo",
    boughtDaysAgo: 7,
  },
  {
    id: "w-treasure",
    edition: "e-treasure",
    title: "Treasure Island",
    authors: "Robert Louis Stevenson",
    narrators: "Iris Vale",
    minutes: 412,
    status: "acquired",
    store: "echo",
    boughtDaysAgo: 200,
  },
  {
    id: "w-war",
    edition: "e-war",
    title: "The War of the Worlds",
    authors: "H. G. Wells",
    narrators: "Theo Marsh",
    minutes: 383,
    status: "acquired",
    store: "libro",
    boughtDaysAgo: 150,
  },
];

/** Books the feed recommends and patrons wish for, not in the library. */
const SUGGESTIONS = [
  { title: "Middlemarch", authors: "George Eliot", narrators: "Iris Vale", minutes: 2120 },
  { title: "The Odyssey", authors: "Homer", narrators: "Theo Marsh", minutes: 690 },
  { title: "Jane Eyre", authors: "Charlotte Brontë", narrators: "Ada Quill", minutes: 1150 },
  {
    title: "Great Expectations",
    authors: "Charles Dickens",
    narrators: "Theo Marsh",
    minutes: 1100,
  },
  { title: "Little Women", authors: "Louisa May Alcott", narrators: "Iris Vale", minutes: 1150 },
  {
    title: "The Picture of Dorian Gray",
    authors: "Oscar Wilde",
    narrators: "Ada Quill",
    minutes: 500,
  },
  { title: "Kidnapped", authors: "Robert Louis Stevenson", narrators: "Theo Marsh", minutes: 520 },
  { title: "The Invisible Man", authors: "H. G. Wells", narrators: "Iris Vale", minutes: 300 },
].map((s, i) => ({ ...s, id: `w-s${i}`, key: `s${i}` }));

/** Cover colors, picked by a stable hash of the cover's key. */
const PALETTE = [
  ["#1f3a5f", "#f3e5c6"],
  ["#7a2e1f", "#f6e7d1"],
  ["#2f5d50", "#eef3e8"],
  ["#5b3a6b", "#f1e6f3"],
  ["#8a6d1d", "#fbf4dc"],
  ["#2b2b2b", "#e9e4d8"],
] as const;

/**
 * A plain generated cover: a colored card with the title, so pages with
 * covers lay out as they do with real ones. No store artwork is used.
 *
 * @param key - The cover's key (an edition id or a cover token).
 * @returns An SVG image.
 */
function cover(key: string): Answer {
  const book = BOOKS.find((b) => b.edition === key);
  const suggestion = SUGGESTIONS.find((s) => s.key === key);
  const title = book?.title ?? suggestion?.title ?? "";
  let hash = 0;
  for (const ch of key) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  const [bg, fg] = PALETTE[hash % PALETTE.length] ?? PALETTE[0];
  const words = title.split(" ");
  const lines: string[] = [];
  for (const word of words) {
    const last = lines.at(-1);
    if (last !== undefined && (last + " " + word).length <= 14)
      lines[lines.length - 1] = `${last} ${word}`;
    else lines.push(word);
  }
  const text = lines
    .slice(0, 5)
    .map(
      (line, i) =>
        `<text x="24" y="${70 + i * 34}" font-family="serif" font-size="28" font-weight="700" fill="${fg}">${line.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</text>`,
    )
    .join("");
  return {
    body: {
      contentType: "image/svg+xml",
      text: `<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300" viewBox="0 0 300 300"><rect width="300" height="300" fill="${bg}"/><rect x="12" y="12" width="276" height="276" fill="none" stroke="${fg}" stroke-opacity="0.4" stroke-width="2"/>${text}</svg>`,
    },
  };
}

/**
 * A storefront's icon: a neutral monogram tile (no store artwork).
 *
 * @param id - Storefront id.
 * @returns An SVG image.
 */
function storeIcon(id: string): Answer {
  const letter = (id[0] ?? "?").toUpperCase();
  return {
    body: {
      contentType: "image/svg+xml",
      text: `<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128"><rect width="128" height="128" rx="24" fill="#2f5d50"/><text x="64" y="86" text-anchor="middle" font-family="sans-serif" font-size="64" font-weight="700" fill="#ffffff">${letter}</text></svg>`,
    },
  };
}

/** The storefronts the sample library uses. */
const SOURCES = [
  {
    id: "echo",
    name: "Echo Books",
    auth: "password",
    brand: { bg: "#2f5d50", fg: "#ffffff", accent: "#7fc8a9", logo: "" },
  },
  {
    id: "libro",
    name: "Libro.fm",
    auth: "password",
    brand: { bg: "#1f3a5f", fg: "#ffffff", accent: "#8fb3e0", logo: "" },
  },
];

/** The signed-in owner. */
const OWNER = {
  id: 1,
  role: "owner",
  display_name: "Robin Reader",
  email: "robin@example.com",
  has_password: true,
  has_avatar: false,
  avatar_source: "initials",
  gravatar_hash: null,
  sso_pictures: [],
};

/** Library members as the user list shows them. */
const USERS = [
  { ...OWNER, status: "active", login_name: "robin", online: true, last_active_at: ago(1) },
  {
    id: 2,
    role: "administrator",
    status: "active",
    display_name: "Sam Shelver",
    login_name: "sam",
    email: "sam@example.com",
    has_password: true,
    has_avatar: false,
    avatar_source: "initials",
    sso_pictures: [],
    online: false,
    last_active_at: ago(3 * 60),
  },
  {
    id: 3,
    role: "member",
    status: "active",
    display_name: "Kit Patron",
    login_name: "kit",
    email: null,
    has_password: true,
    has_avatar: false,
    avatar_source: "initials",
    sso_pictures: [],
    online: false,
    last_active_at: ago(2 * DAY),
  },
].map((u) => ({
  last_seen_at: u.last_active_at,
  listening: null,
  integrations: u.id === 1 ? [{ source: "echo", account_id: "robin@echo", label: "Robin" }] : [],
  controls: {
    link_accounts: true,
    download: u.role !== "member",
    edit_metadata: u.role !== "member",
    upload: true,
    request_books: true,
    see_prices: true,
  },
  identities: [],
  ...u,
}));

/** Node runtime status for a live node. */
function nodeStatus(id: string, cpu: number, guests: number) {
  return {
    node_id: id,
    incarnation: `${id}-1`,
    started_at: ago(6 * DAY),
    heartbeat_at: ago(0),
    heartbeat_ms: NOW_MS,
    interval_ms: 15_000,
    cpu_millicores: 4000,
    memory_bytes: 8 * 2 ** 30,
    cpu_budget_millicores: 3000,
    memory_budget_bytes: 6 * 2 ** 30,
    schedulable: true,
    cpu_used_millicores: cpu,
    memory_available_bytes: 5 * 2 ** 30,
    files_dir_free_bytes: 420 * 2 ** 30,
    reserved: { memory_bytes: 512 * 2 ** 20, cpu_millicores: 500 },
    running_guests: guests,
    availability_ppm: 999_200,
    online_since_ms: NOW_MS - 6 * DAY * 60_000,
    cache_bytes: 3 * 2 ** 30,
    cache_budget_bytes: 10 * 2 ** 30,
    cache_swept_ms: NOW_MS - 3_600_000,
    public_origin: null,
  };
}

/** The library's nodes: this server and a patron's branch. */
const NODES = [
  {
    node_id: "node-main",
    library_id: "lib-1",
    created_at: ago(400 * DAY),
    heartbeat_at: ago(0),
    software_version: "0.4.0",
    schema_state: "current",
    compatible: true,
    target: "x86_64-unknown-linux-gnu",
    labels: ["storage"],
    implicit_labels: ["os:linux", "arch:x86_64"],
    live: true,
    runtime: {
      label: "Living room server",
      cpu_budget_percent: 75,
      memory_budget_percent: 75,
      schedulable: true,
    },
    status: nodeStatus("node-main", 820, 3),
    this_node: true,
    kind: "shared",
    relationship: "shared_branch",
    owner_user_id: 1,
    owner: { user_id: 1, display_name: "Robin Reader", role: "owner" },
  },
  {
    node_id: "node-kit",
    library_id: "lib-1",
    created_at: ago(30 * DAY),
    heartbeat_at: ago(1),
    software_version: "0.4.0",
    schema_state: "current",
    compatible: true,
    target: "aarch64-apple-darwin",
    labels: [],
    implicit_labels: ["os:macos", "arch:aarch64"],
    live: true,
    runtime: {
      label: "Kit's laptop",
      cpu_budget_percent: 25,
      memory_budget_percent: 25,
      schedulable: true,
    },
    status: nodeStatus("node-kit", 140, 1),
    this_node: false,
    kind: "branch",
    relationship: "remote_branch",
    owner_user_id: 3,
    share_books: true,
    include_branches: false,
    owner: { user_id: 3, display_name: "Kit Patron", role: "member" },
  },
];

/** What every node list says the viewer may do. */
const NODE_CAPABILITIES = {
  scope: "all",
  can_edit_runtime: true,
  can_edit_labels: true,
  can_assign_owner: true,
};

/** A recommendation or wish for one suggestion. */
function suggestion(s: (typeof SUGGESTIONS)[number], i: number) {
  return {
    work_id: s.id,
    work_key: s.key,
    title: s.title,
    authors: s.authors,
    narrators: s.narrators,
    score: 0.9 - i * 0.05,
    reasons: [i % 2 ? "Narrated by someone you listen to" : "Popular with patrons"],
    purchase_hints: [
      {
        source: i % 2 ? "libro" : "echo",
        product_id: `p-${s.key}`,
        price_cents: 1499 + i * 100,
        currency: "USD",
        price_label: `$${(14.99 + i).toFixed(2)}`,
      },
    ],
    from_request: false,
    store_editions: [{ source: i % 2 ? "libro" : "echo", product_id: `p-${s.key}` }],
    cover_path: `/api/covers/${s.key}`,
    length_minutes: s.minutes,
    language: "en",
    wishers: [],
    wish_count: 0,
    // Kit's branch has the first suggestion: its details offer to ask for it.
    available_through:
      i === 0
        ? [
            {
              work_id: s.id,
              node_id: "node-kit",
              owner_name: "Kit",
              store: "libro",
              product_id: `p-${s.key}`,
            },
          ]
        : [],
  };
}

/** Sessions the Account tab lists. */
const SESSIONS = [
  {
    kind: "portal",
    id: 11,
    created_at: ago(3 * DAY),
    expires_at: "2026-02-14T12:00:00Z",
    last_used_at: ago(0),
    is_current: true,
    client_label: "Firefox on Linux",
    device_type: "desktop",
    user_agent: null,
    channel: "browser",
  },
  {
    kind: "portal",
    id: 12,
    created_at: ago(10 * DAY),
    expires_at: "2026-02-04T12:00:00Z",
    last_used_at: ago(5 * 60),
    is_current: false,
    client_label: "Bookclerk on iPhone",
    device_type: "phone",
    user_agent: null,
    channel: "clerk",
  },
];

/** Every title the samples know: the library's books and the suggestions. */
const TITLES = [
  ...BOOKS.map((b) => ({ ...b, key: b.edition, store: b.store as string })),
  ...SUGGESTIONS.map((s, i) => ({ ...s, store: i % 2 ? "libro" : "echo" })),
];

/**
 * The queries of a batched lookup body.
 *
 * @param body - The request body.
 * @returns Each query's title.
 */
function queries(body: unknown): string[] {
  const list = (body as { queries?: { title?: unknown }[] } | null)?.queries ?? [];
  return list.map((q) => (typeof q.title === "string" ? q.title : ""));
}

/**
 * The store price of a title, when the samples have one.
 *
 * @param title - The title asked about.
 * @returns Its purchase hints and the best one.
 */
function hintsFor(title: string) {
  const t = TITLES.find((x) => x.title === title);
  if (!t) return { hints: [], best: null, incomplete: false };
  const hint = {
    source: t.store,
    product_id: `p-${t.key}`,
    title: t.title,
    price_cents: 1499,
    currency: "USD",
    price_label: "$14.99",
    member_price_cents: 1199,
    member_price_label: "$11.99",
  };
  return { hints: [hint], best: hint, incomplete: false };
}

/**
 * Store metadata for a title, when the samples have one.
 *
 * @param title - The title asked about.
 * @returns Its metadata, or null.
 */
function metaFor(title: string) {
  const t = TITLES.find((x) => x.title === title);
  if (!t) return null;
  return {
    title: t.title,
    authors: t.authors,
    narrators: t.narrators,
    cover_path: `/api/covers/${t.key}`,
    description: `${t.title} by ${t.authors}, read in full by ${t.narrators}. A public-domain classic in a new unabridged recording.`,
    publisher: "Echo Audio",
    length_minutes: t.minutes,
    published_at: "2024-03-01",
    categories: "Fiction, Classics",
    language: "en",
    is_abridged: false,
    rating_overall: 4.5,
    rating_performance: 4.6,
    rating_story: 4.4,
    rating_count: 1280,
    review_count: 0,
    reviews: [],
  };
}

/**
 * A work's detail page data.
 *
 * @param id - Work id.
 * @returns The detail, or null for an unknown work.
 */
function workDetail(id: string) {
  const b = BOOKS.find((x) => x.id === id);
  if (!b) return null;
  return {
    id: b.id,
    kind: "book",
    title: b.title,
    description: metaFor(b.title)?.description ?? null,
    authors: b.authors.split(", "),
    series: b.series ?? null,
    series_position: b.position ?? null,
    editions: [
      {
        id: b.edition,
        work_id: b.id,
        expression_id: `x-${b.id}`,
        expression_kind: "narration",
        language: "en",
        abridgement: "unabridged",
        media_kind: "audio",
        title: b.title,
        published_at: "2024-03-01",
        duration_minutes: b.minutes,
        acquire_status: b.status,
        listings: [
          {
            id: `l-${b.id}`,
            plugin_key: b.store,
            marketplace: "us",
            product_id: `p-${b.edition}`,
            url: null,
            holdings: [
              {
                id: `h-${b.id}`,
                holder: { kind: "account", plugin_key: b.store, account_id: `robin@${b.store}` },
                entitlement: "purchase",
                purchased_at: ago(b.boughtDaysAgo * DAY),
                added_at: ago(b.boughtDaysAgo * DAY),
              },
            ],
          },
        ],
      },
    ],
    state: {
      is_finished: false,
      rating_overall: 4,
      rating_performance: 5,
      rating_story: 4,
      tags: null,
    },
    can_edit_state: true,
  };
}

/**
 * What the player gets for an edition: one track, the listener part way
 * through, two bookmarks and five chapters.
 *
 * @param editionId - The edition.
 * @returns Its playback manifest.
 */
function manifest(editionId: string) {
  const b = BOOKS.find((x) => x.edition === editionId) ?? BOOKS[1];
  const duration = (b?.minutes ?? 60) * 60_000;
  return {
    edition_id: editionId,
    work_id: b?.id ?? null,
    title: b?.title ?? "",
    playable: true,
    duration_ms: duration,
    tracks: [
      {
        index: 0,
        path: `/api/playback/sessions/play-1/stream/0`,
        content_type: "audio/mpeg",
        bitrate_kbps: 64,
        lossless: false,
      },
    ],
    progress: {
      edition_id: editionId,
      work_id: b?.id ?? null,
      position_ms: Math.round(duration * 0.42),
      duration_ms: duration,
      is_finished: false,
      started_at: ago(3 * DAY),
      updated_at: ago(20),
    },
    bookmarks: [
      {
        id: "bm-1",
        edition_id: editionId,
        position_ms: Math.round(duration * 0.1),
        title: "Chapter 5 opens",
        created_at: ago(2 * DAY),
      },
      {
        id: "bm-2",
        edition_id: editionId,
        position_ms: Math.round(duration * 0.38),
        title: "The storm",
        created_at: ago(DAY),
      },
    ],
    // Five chapters of equal length, the last ending where the book does.
    chapters: ["Jonathan Harker's Journal", "The Castle", "Lucy", "Whitby", "The Hunt"].map(
      (title, i) => ({
        title,
        start_ms: Math.round((duration * i) / 5),
        end_ms: Math.round((duration * (i + 1)) / 5),
        depth: 0,
      }),
    ),
  };
}

/**
 * The plugins Settings lists: an S3 destination the library scheduler
 * places on two storage nodes (expanded on one page to show its settings,
 * a table among them), the local folder, and a storefront not yet deployed
 * whose network access needs approving.
 */
const PLUGINS = [
  {
    id: "s3",
    family: "destination",
    plugin_key: "s3",
    provenance: "bundled",
    display_name: "S3 storage",
    plugin_instance_id: "s3-default",
    is_default: true,
    config_revision: 4,
    deployment: {
      deployment_id: "d-s3",
      node_id: "node-main",
      desired: "placed",
      revision: 4,
      status: "running",
      applied_config_revision: 4,
      observed_at: ago(1),
    },
    placement: {
      replicas: 2,
      constraints: ["storage"],
      revision: 2,
      placed: 1,
      detail: "1 of 2 placed: only one live node has the storage label",
      scheduled_at: ago(30),
      updated_at: ago(30),
    },
    storage_sync: {
      feed_state: "healthy",
      feed_checked_at: ago(2),
      listed_at: ago(6 * 60),
      next_listing_at: "2026-01-16T06:00:00Z",
      listing_in_progress: false,
      objects: 1284,
    },
    present_on: ["node-main"],
    settings: [
      { key: "bucket", label: "Bucket", value: "robin-audiobooks", value_type: "string" },
      {
        key: "region",
        label: "Region",
        value: "eu-central-1",
        value_type: "string",
        choices: [
          { value: "us-east-1", label: "US East (N. Virginia)" },
          { value: "eu-central-1", label: "EU (Frankfurt)" },
        ],
      },
      {
        key: "secret_access_key",
        label: "Secret access key",
        value: "",
        value_type: "string",
        secret: true,
        secret_set: true,
      },
      {
        key: "prefixes",
        label: "Folder per collection",
        value_type: "table",
        value: JSON.stringify([
          { collection: "Classics", prefix: "classics/" },
          { collection: "Mysteries", prefix: "mysteries/" },
        ]),
        columns: [
          { key: "collection", label: "Collection", value_type: "string", required: true },
          { key: "prefix", label: "Prefix", value_type: "string", default: "books/" },
        ],
      },
    ],
  },
  {
    id: "local",
    family: "destination",
    plugin_key: "local",
    provenance: "bundled",
    display_name: "Local folder",
    plugin_instance_id: "local-default",
    is_default: true,
    config_revision: 1,
    deployment: {
      deployment_id: "d-local",
      node_id: "node-main",
      desired: "present",
      revision: 1,
      status: "running",
      applied_config_revision: 1,
      observed_at: ago(1),
    },
    storage_sync: {
      feed_state: "gap",
      feed_error: "The folder watcher missed events; a full listing is queued.",
      feed_checked_at: ago(5),
      listed_at: ago(DAY),
      listing_in_progress: true,
      objects: 312,
    },
    present_on: ["node-main"],
    settings: [{ key: "root", label: "Folder", value: "/srv/audiobooks", value_type: "string" }],
  },
  {
    id: "echo",
    family: "source",
    plugin_key: "echo",
    provenance: "bundled",
    display_name: "Echo Books",
    plugin_instance_id: "echo-default",
    is_default: true,
    config_revision: 1,
    deployment: null,
    present_on: [],
    settings: [],
  },
];

/**
 * The S3 destination's guided setup: pick a bucket and region, paste a key
 * that may create buckets, and it fills in the rest.
 */
const S3_SETUP = {
  provider: "aws",
  grants: ["api_token"],
  token_url: "https://console.aws.amazon.com/iam/home#/security_credentials",
  token_permissions: ["s3:CreateBucket", "iam:CreateAccessKey"],
  inputs: ["bucket", "region"],
  outputs: ["bucket", "region", "secret_access_key"],
  deprovision: true,
};

/**
 * What the collector holds for a sent diagnostics report.
 *
 * @param id - The report id.
 * @returns The lookup answer.
 */
function reportLookup(id: string) {
  const sent = NOW_MS - 2 * DAY * 60_000;
  return {
    report_id: id,
    received_at_unix_ms: sent + 2000,
    report: {
      version: "0.4.0",
      trigger: "error_burst",
      os: "linux",
      arch: "x86_64",
      distro: "Debian 13",
      events: [
        {
          ts_unix_ms: sent - 4000,
          level: "WARN",
          target: "bookclerk_library::acquire",
          message: "store answered 503, retrying",
          fields: [{ name: "attempt", value: "2" }],
        },
        {
          ts_unix_ms: sent - 1000,
          level: "ERROR",
          target: "bookclerk_library::acquire",
          message: "acquire failed",
          fields: [{ name: "error_kind", value: "store_unavailable" }],
        },
      ],
    },
    issues: [
      {
        number: 1042,
        title: "Acquire gives up while the store is briefly unavailable",
        state: "open",
        url: "https://github.com/example/bookclerk/issues/1042",
      },
    ],
    fix: null,
  };
}

/** The storefront's network request, which nobody approved yet. */
const ECHO_CONSENT = {
  plugin_id: "echo",
  plugin_key: "echo",
  provenance: "bundled",
  runtime: "workerd",
  request: {
    pluginKey: "echo",
    pluginId: "echo",
    entrypoints: ["catalog", "library", "download"],
    producers: [],
    consumers: [],
    jobs: ["scan"],
    networkMode: "domains",
    domains: ["api.echo.example", "cdn.echo.example"],
    manifestDomains: ["api.echo.example", "cdn.echo.example"],
    operatorAddedDomains: [],
    operatorDeniedDomains: [],
    tcp: [],
    operatorAddedTcp: [],
    operatorDeniedTcp: [],
    addressCidrs: [],
    operatorAddedCidrs: [],
    operatorDeniedCidrs: [],
    allowUndeclaredPublicRedirects: false,
    grantRevision: "r1",
    authorityRevision: "a1",
    bindings: [],
    compatibilityFlags: [],
    approvedAt: "",
  },
  covered: false,
  summary: ["Reach api.echo.example and cdn.echo.example", "Run a library scan job"],
  existing: null,
  pending: null,
  brand: { name: "Echo Books", bg: "#2f5d50", fg: "#ffffff", accent: "#7fc8a9", logo: null },
  limits: {
    cpu_ms: 50,
    subrequests: 50,
    max_cpu_ms: 30_000,
    max_subrequests: 1000,
    disk_mib: 64,
    max_disk_mib: 1024,
    memory_mib: 128,
    max_memory_mib: 1024,
    cpu_cores: 0.5,
    max_cpu_cores: 4,
    extra_processes: 0,
    max_extra_processes: 4,
    known_bindings: [],
  },
  host_user_agent: "Bookclerk/0.4.0",
  setting_destinations: [],
};

/**
 * A stand-in for the authenticator QR code: a fixed pattern of squares in
 * a QR code's shape (it encodes nothing).
 */
const QR_SVG = (() => {
  const cells: string[] = [];
  for (let y = 0; y < 25; y++)
    for (let x = 0; x < 25; x++) {
      const finder = (x < 7 || x > 17) && y < 7 ? true : x < 7 && y > 17;
      const on = finder
        ? !(x % 6 && y % 6) || (x % 18 >= 2 && x % 18 <= 4 && y % 18 >= 2 && y % 18 <= 4)
        : (x * 7 + y * 13 + x * y) % 3 === 0;
      if (on) cells.push(`M${x} ${y}h1v1h-1z`);
    }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="-2 -2 29 29"><rect x="-2" y="-2" width="29" height="29" fill="#fff"/><path d="${cells.join("")}" fill="#000"/></svg>`;
})();

/**
 * A sample book as a row of `GET /api/library/works`.
 *
 * @param b - The book.
 * @returns Its work summary.
 */
function workSummary(b: Book): Record<string, unknown> {
  return {
    id: b.id,
    kind: "book",
    title: b.title,
    authors: b.authors,
    series: b.series ?? null,
    series_position: b.position ?? null,
    cover_edition_id: b.edition,
    media_kinds: ["audio"],
    acquire_status: b.status,
    edition_count: 1,
    holding_count: b.status === "acquired" ? 1 : 0,
    acquirable_editions: b.status === "not_acquired" || b.status === "error" ? 1 : 0,
    added_at: ago(b.boughtDaysAgo * DAY),
    length_minutes: b.minutes,
    purchased_at: ago(b.boughtDaysAgo * DAY),
  };
}

/** Answers by `METHOD /path` for one viewer. */
export type Routes = Record<string, (body: unknown) => Answer>;

/**
 * Answers every signed-in viewer gets.
 *
 * @param viewer - Owner or operator.
 * @returns The routes.
 */
function signedIn(viewer: "owner" | "operator"): Routes {
  const operator = viewer === "operator";
  return {
    "GET /api/auth/me": () => ({
      json: {
        authenticated: true,
        role: viewer,
        default_view: "library",
        can_acquire: true,
        elevated: true,
        administers_nodes: true,
        user: operator ? null : OWNER,
        second_factor: operator
          ? null
          : { required: false, totp: true, passkey_count: 1, enrolled: true },
        see_prices: true,
      },
    }),
    "GET /api/preferences": () => ({
      json: {
        default_view: "library",
        disabled_shelves: [],
        discover_sort: "score",
        discover_sort_dir: "desc",
        discover_language: "en",
        discover_excluded_sources: [],
        theme: "system",
      },
    }),
    "GET /api/portal/sources": () => ({ json: { sources: SOURCES } }),
    "GET /api/discover/recommendations": () => ({
      json: {
        shelves: [
          {
            id: "for-you",
            title: "Picked for you",
            subtitle: "From what you finished lately",
            items: SUGGESTIONS.slice(0, 5).map(suggestion),
          },
          {
            id: "more-by",
            title: "More by authors you own",
            subtitle: null,
            items: SUGGESTIONS.slice(5).map((s, i) => suggestion(s, i + 5)),
          },
        ],
        shelf_kinds: [
          { id: "for-you", label: "Picked for you" },
          { id: "more-by", label: "More by authors you own" },
        ],
        state: "ready",
        computed_at: ago(30),
      },
    }),
    "GET /api/wishlist": () => ({
      json: SUGGESTIONS.slice(1, 3).map((s, i) => ({
        id: 100 + i,
        uuid: `00000000-0000-4000-8000-00000000010${i}`,
        title: s.title,
        authors: s.authors,
        narrators: s.narrators,
        status: "open",
        work_key: s.key,
        work_id: s.id,
        cover_path: `/api/covers/${s.key}`,
        length_minutes: s.minutes,
        language: "en",
        sources: [],
        store_editions: [{ source: "echo", product_id: `p-${s.key}` }],
        purchase_hints: [
          {
            source: "echo",
            product_id: `p-${s.key}`,
            price_cents: 1299,
            currency: "USD",
            price_label: "$12.99",
          },
        ],
        created_at: ago((i + 2) * DAY),
        updated_at: ago((i + 2) * DAY),
      })),
    }),
    "GET /api/request-queue": () => ({
      json: SUGGESTIONS.slice(1, 4).map((s, i) => ({
        work_key: s.key,
        work_id: s.id,
        title: s.title,
        authors: s.authors,
        cover_path: `/api/covers/${s.key}`,
        wish_count: 3 - i,
        wishers: USERS.slice(0, 3 - i).map((u) => ({
          user_id: u.id,
          display_name: u.display_name,
          login_name: u.login_name,
          operator: false,
          has_avatar: false,
          avatar_source: "initials",
        })),
        sample_uuids: [`00000000-0000-4000-8000-00000000020${i}`],
        first_requested_at: ago((10 - i) * DAY),
        last_requested_at: ago((i + 1) * DAY),
        score: 3 - i,
        taste_score: 0.8 - i * 0.1,
        reasons: ["Wished for by patrons"],
      })),
    }),
    "GET /api/library/works": () => ({
      json: { works: BOOKS.map(workSummary), next_cursor: null },
    }),
    "GET /api/library/works/meaning": () => ({
      json: { search: "ready", similar: "ready" },
    }),
    "GET /api/playback/progress": () => ({
      json: {
        items: [
          { book: BOOKS[1], position: 0.42 },
          { book: BOOKS[4], position: 0.8 },
        ].map(({ book, position }, i) => ({
          edition_id: book?.edition ?? "",
          work_id: book?.id ?? null,
          position_ms: Math.round((book?.minutes ?? 0) * 60_000 * position),
          duration_ms: (book?.minutes ?? 0) * 60_000,
          is_finished: false,
          started_at: ago((i + 3) * DAY),
          updated_at: ago(i * 60 + 20),
        })),
        next_cursor: null,
      },
    }),
    "GET /api/config/join/window": () => ({ json: { libraries: [], cards: [], books: [] } }),
    "GET /api/status": () => ({
      json: {
        accounts: 2,
        books: BOOKS.length,
        acquired: BOOKS.filter((b) => b.status === "acquired").length,
        pending: 1,
        error: 1,
        in_progress: 1,
        listen: "127.0.0.1:8787",
        storage_backend: "local",
        auth_enabled: true,
        events: {
          pending: 0,
          running: 1,
          suspended: 0,
          dead_letter: 0,
          acked: 1840,
          oldest_pending_age_secs: null,
          retries_total: 3,
          suspensions_total: 0,
          dead_letters_total: 0,
          dispatch_latency_ms_avg: 12,
          handler_latency_ms_avg: 140,
        },
      },
    }),
    "GET /api/jobs": () => ({
      json: [
        {
          id: "job-3",
          kind: "acquire",
          status: "running",
          detail: "Frankenstein; or, The Modern Prometheus",
          progress: "62%",
          attempt_count: 1,
          max_attempts: 3,
          created_at: ago(4),
        },
        {
          id: "job-2",
          kind: "acquire",
          status: "failed",
          detail: "The Time Machine",
          attempt_count: 3,
          max_attempts: 3,
          error_kind: "store_unavailable",
          created_at: ago(70),
          finished_at: ago(40),
        },
        {
          id: "job-1",
          kind: "scan",
          status: "succeeded",
          detail: "Echo Books",
          attempt_count: 1,
          max_attempts: 3,
          created_at: ago(90),
          finished_at: ago(88),
        },
      ],
    }),
    "GET /api/events/deliveries": () => ({ json: [] }),
    "GET /api/config/join/sharing": () => ({ json: { libraries: [] } }),
    "GET /api/portal/connections": () => ({
      json: {
        connections: [
          {
            account_id: "robin@echo",
            source: "echo",
            plugin_key: "echo",
            label: "Robin",
            connection_status: "connected",
            source_enabled: true,
            brand: SOURCES[0]?.brand ?? null,
            pace: {
              mode: "store",
              downloads_per_day: 20,
              min_gap_seconds: 60,
              store_downloads_per_day: 20,
              store_min_gap_seconds: 60,
              downloads_last_day: 3,
              next_automatic_at: "2026-01-15T13:00:00Z",
            },
          },
          {
            account_id: "robin@libro",
            source: "libro",
            plugin_key: "libro",
            label: null,
            connection_status: "needs_login",
            source_enabled: true,
            brand: SOURCES[1]?.brand ?? null,
            pace: {
              mode: "store",
              downloads_per_day: 10,
              min_gap_seconds: 120,
              downloads_last_day: 0,
            },
          },
        ],
      },
    }),
    "GET /api/library-status": () => ({
      json: {
        library: {
          nodes: 2,
          live: 2,
          drained: 0,
          branch_nodes: 1,
          cpu_budget_millicores: 4000,
          cpu_reserved_millicores: 1000,
          memory_budget_bytes: 8 * 2 ** 30,
          memory_reserved_bytes: 2 ** 30,
          running_guests: 4,
          active_jobs: 1,
          deliveries: { pending: 0, running: 1, dead_letter: 0, oldest_pending_age_secs: null },
          leases: [
            {
              name: "scheduler",
              holder: "node-main",
              token: 41,
              held: true,
              expires_at_ms: NOW_MS + 30_000,
            },
          ],
          schema: { state: "current", compatible: true, error: null },
        },
        node_id: "node-main",
        nodes: NODES,
        capabilities: NODE_CAPABILITIES,
      },
    }),
    "GET /api/auth/profile/catalog-listing": () => ({
      json: { listed: true, libraries: 1, always_listed: false },
    }),
    "GET /api/auth/passkeys": () => ({
      json: { passkeys: [{ id: 1, credential_id: "cred-1", name: "Laptop fingerprint reader" }] },
    }),
    "GET /api/auth/oidc/identities": () => ({ json: { identities: [] } }),
    "GET /api/auth/oidc/providers": () => ({ json: { enabled: false, providers: [] } }),
    "GET /api/config/master-key": () => ({
      json: {
        store: "keychain",
        recovery_copy: "saved",
        recorded_at: ago(90 * DAY),
        needs_recovery_copy: false,
      },
    }),
    "GET /api/users": () => ({ json: { users: USERS } }),
    "GET /api/auth/sessions": () => ({ json: { sessions: SESSIONS } }),
    "GET /api/users/default-controls": () => ({
      json: {
        controls: {
          link_accounts: true,
          download: false,
          edit_metadata: false,
          upload: true,
          request_books: true,
          see_prices: true,
        },
      },
    }),
    "GET /api/users/default-scope": () => ({
      json: { scope: { rules: [] }, version: "0123456789abcdef0123456789abcdef" },
    }),
    "GET /api/library/collections": () => ({
      json: { collections: [{ name: "Classics", prefixes: ["Classics/"] }] },
    }),
    "GET /api/users/default-admin-controls": () => ({
      json: {
        controls: {
          manage_members: true,
          set_patron_limits: true,
          library_scan: true,
          run_jobs: true,
          fix_catalog: true,
          invite_branches: false,
          approve_requests: true,
        },
      },
    }),
    "GET /api/auth/oidc/config": () => ({
      json: {
        enabled: false,
        allowed_email_domains: [],
        public_origin: "https://books.example.org",
        issuer_url: "https://books.example.org",
        detected_origin: "https://books.example.org",
        discovery_url: "https://books.example.org/.well-known/openid-configuration",
        authorization_endpoint: "https://books.example.org/oauth/authorize",
        token_endpoint: "https://books.example.org/oauth/token",
        userinfo_endpoint: "https://books.example.org/oauth/userinfo",
        revocation_endpoint: "https://books.example.org/oauth/revoke",
        callback_url: "https://books.example.org/api/auth/oidc/callback",
        providers: [],
      },
    }),
    "GET /api/auth/mfa-policy": () => ({ json: { require_second_factor: false } }),
    "GET /api/auth/oidc/clients": () => ({ json: { clients: [] } }),
    "GET /api/config/nodes": () => ({
      json: { node_id: "node-main", nodes: NODES, capabilities: NODE_CAPABILITIES },
    }),
    "GET /api/config/clerk": () => ({
      json: { cards: [], default_clerk_name: "Living room server" },
    }),
    "GET /api/config/library-consents": () => ({ json: { libraries: [], consents: [] } }),
    "GET /api/config/branches": () => ({
      json: {
        library_name: "Robin's library",
        origin: "https://books.example.org",
        transport: "websocket",
        enrollment: true,
        spki_fingerprint: null,
        branches: [
          {
            node_id: "node-kit",
            created_at: ago(30 * DAY),
            owner_user_id: 3,
            owner_name: "Kit Patron",
            joined: true,
            share_books: true,
            include_branches: false,
            acquire_members_books: false,
            titles: { shared: 4, delivered: 2, detached: 0 },
          },
        ],
      },
    }),
    "GET /api/config/shared-branches": () => ({
      json: { node_id: "node-main", nodes: NODES.filter((n) => n.kind === "shared") },
    }),
    "GET /api/config/join": () => ({
      json: {
        fresh: false,
        has_owner: true,
        shared_branch_eligible: true,
        shared_branch_problem: null,
        default_branch_name: "Living room server",
        links: [],
      },
    }),
    "POST /api/discover/purchase-hints": (body) => ({
      json: hintsFor((body as { title?: string } | null)?.title ?? ""),
    }),
    "POST /api/discover/purchase-hints/batch": (body) => ({
      json: { results: queries(body).map(hintsFor) },
    }),
    "POST /api/discover/title-meta/batch": (body) => {
      const titles = queries(body);
      return { json: { results: titles.map(metaFor), incomplete: titles.map(() => false) } };
    },
    "GET /api/diagnostics/reports": () => ({ json: { reports: [] } }),
    "POST /api/playback/sessions": (body) => {
      const id = (body as { edition_id?: string } | null)?.edition_id ?? "";
      const m = manifest(id);
      return {
        json: {
          session_id: "play-1",
          edition_id: id,
          work_id: m.work_id,
          position_ms: m.progress?.position_ms ?? 0,
          duration_ms: m.duration_ms,
          tracks: m.tracks,
          idle_secs: 300,
        },
      };
    },
    "GET /api/config/node-invites": () => ({
      json: {
        links: [
          {
            link_id: "inv-1",
            for_user_id: 2,
            label: "Sam's desktop",
            created_by: "Robin Reader",
            created_at: ago(60),
            expires_at: "2026-01-16T11:00:00Z",
            state: "open",
          },
        ],
      },
    }),
    "GET /api/auth/branch-consent": () => ({
      json: {
        library_name: "Robin's library",
        branch_name: "Sam's desktop",
        redirect_origin: "https://sam-desktop.example.org",
        enrollment: true,
        user: { id: 1, display_name: "Robin Reader" },
        connection: {
          choice: true,
          default: "branch",
          shared_branch_available: true,
          step_up: ["passkey", "totp"],
        },
        connections: [
          { source: "echo", account_id: "robin@echo", label: "Robin", travels: true },
          { source: "libro", account_id: "robin@libro", label: null, travels: false },
        ],
      },
    }),
    "POST /api/config/join/callback": () => ({
      json: {
        library: {
          library_id: "lib-ana",
          owner_origin: "https://ana.example.net",
          node_id: "node-main",
          pinned: true,
          state: "joined",
          enrolled_at: ago(0),
          share_books: true,
          include_branches: false,
          work_for_members: false,
          keep_member_copies: false,
          list_members_books: true,
          keep_copies: false,
          running: true,
        },
        library_name: "Ana's library",
        running: true,
        connections: { offered: 2, added: 2, already_here: 0, taken: 0, failed: 0 },
        replaced_card: null,
      },
    }),
    "POST /api/config/clerk/callback": () => ({
      json: {
        card: {
          library_id: "lib-ana",
          origin: "https://ana.example.net",
          library_name: "Ana's library",
          pinned: true,
          display_name: "Robin",
          clerk_name: "Living room server",
          state: "signed_in",
          signed_in_at: ago(0),
        },
        left_branch: null,
      },
    }),
    "GET /api/auth/player-passwords": () => ({
      json: {
        passwords: [
          {
            id: "pp-1",
            label: "Car stereo app",
            created_at: ago(20 * DAY),
            last_used_at: ago(DAY),
          },
        ],
      },
    }),
    "GET /api/auth/player-consent": () => ({
      json: { app_name: "Shelf Player", display_name: "Robin Reader" },
    }),
    "GET /api/auth/clerk-consent": () => ({
      json: {
        library_name: "Robin's library",
        clerk_name: "Bookclerk on iPhone",
        redirect_origin: "https://clerk.example.org",
        user: { id: 1, display_name: "Robin Reader" },
      },
    }),
    "GET /api/discover/search": () => ({
      json: {
        items: SUGGESTIONS.filter((s) => s.authors === "Robert Louis Stevenson")
          .concat(SUGGESTIONS.slice(0, 2))
          .map((s, i) => ({
            work_key: s.key,
            title: s.title,
            authors: s.authors,
            narrators: s.narrators,
            cover_path: `/api/covers/${s.key}`,
            store_editions: [{ source: i % 2 ? "libro" : "echo", product_id: `p-${s.key}` }],
            sources: [i % 2 ? "libro" : "echo"],
            length_minutes: s.minutes,
            language: "en",
            genres: "Fiction, Classics",
            rating_overall: 4.3,
            rating_count: 410,
            purchase_hints: [
              {
                source: i % 2 ? "libro" : "echo",
                product_id: `p-${s.key}`,
                price_cents: 1499,
                currency: "USD",
                price_label: "$14.99",
              },
            ],
          })),
        page_size: 24,
        has_more: false,
        next_cursor: null,
        sort: "relevance",
      },
    }),
    "GET /api/settings": () => ({
      json: {
        settings: { "media.isolation": "required", "plugins.isolation": "required" },
        effective: {
          "media.isolation": "required",
          "plugins.isolation": "required",
          "daemon.listen": "127.0.0.1:8787",
        },
        plugins: PLUGINS,
        host_cpu_cores_max: 4,
        jail_cpu_cores: null,
        output_revision: 1,
      },
    }),
  };
}

/** Answers for a visitor who has not signed in. */
const SIGNED_OUT: Routes = {
  // The daemon's signed-out answer: the same body, `authenticated` false.
  "GET /api/auth/me": () => ({
    status: 401,
    json: {
      authenticated: false,
      default_view: "discover",
      can_acquire: false,
      elevated: false,
      administers_nodes: false,
      see_prices: false,
    },
  }),
  "GET /api/auth/signin": () => ({
    json: {
      operator_token: true,
      oidc: [],
      integrations: [{ id: "echo", name: "Echo Books" }],
      require_second_factor: false,
    },
  }),
};

/** Image routes, the same for every viewer. */
const IMAGES: [RegExp, (match: RegExpExecArray) => Answer][] = [
  [/^\/api\/library\/editions\/([^/]+)\/cover$/, (m) => cover(decodeURIComponent(m[1] ?? ""))],
  [/^\/api\/covers\/([^/]+)(?:\/(?:\d+|full))?$/, (m) => cover(decodeURIComponent(m[1] ?? ""))],
  [/^\/api\/stores\/([^/]+)\/icon$/, (m) => storeIcon(decodeURIComponent(m[1] ?? ""))],
];

/**
 * The sample answer to one API request.
 *
 * @param viewer - Who the page is rendered for.
 * @param method - HTTP method.
 * @param pathname - Request path.
 * @param body - The request's JSON body, if any.
 * @param scenario - A variation of the sample library the page shows.
 * @returns The answer, or undefined when the samples have none (the test
 *   then fails and names the request).
 */
export function answer(
  viewer: Viewer,
  method: string,
  pathname: string,
  body: unknown = null,
  scenario?: Scenario,
): Answer | undefined {
  const variation: Routes | undefined = scenario ? SCENARIOS[scenario] : undefined;
  const special = variation?.[`${method} ${pathname}`];
  if (special) return special(body);
  if (method === "GET") {
    for (const [pattern, make] of IMAGES) {
      const m = pattern.exec(pathname);
      if (m) return make(m);
    }
  }
  if (viewer !== "signed-out") {
    const tags = /^\/api\/library\/works\/([^/]+)\/tags$/.exec(pathname);
    if (method === "GET" && tags) {
      const id = decodeURIComponent(tags[1] ?? "");
      return {
        json: {
          work_id: id,
          tags: [
            { value: "Classics", source: "admin" },
            { value: "Sea stories", source: "sidecar" },
          ],
        },
      };
    }
    const consent = /^\/api\/plugins\/([^/]+)\/consent$/.exec(pathname);
    if (method === "GET" && consent) {
      const id = decodeURIComponent(consent[1] ?? "");
      // Only the storefront still waits for approval.
      return { json: { ...ECHO_CONSENT, plugin_id: id, plugin_key: id, covered: id !== "echo" } };
    }
    const setup = /^\/api\/config\/plugin-instances\/([^/]+)\/provisioning$/.exec(pathname);
    if (method === "GET" && setup) {
      const id = decodeURIComponent(setup[1] ?? "");
      // Only the S3 destination offers a guided setup.
      return {
        json: { plugin_instance_id: id, provisioning: id === "s3-default" ? S3_SETUP : null },
      };
    }
    const report = /^\/api\/diagnostics\/reports\/([^/]+)$/.exec(pathname);
    if (method === "GET" && report)
      return { json: reportLookup(decodeURIComponent(report[1] ?? "")) };
    const playback = /^\/api\/library\/editions\/([^/]+)\/playback$/.exec(pathname);
    if (method === "GET" && playback) {
      return { json: manifest(decodeURIComponent(playback[1] ?? "")) };
    }
    const similar = /^\/api\/library\/works\/([^/]+)\/similar$/.exec(pathname);
    if (method === "GET" && similar) {
      const id = decodeURIComponent(similar[1] ?? "");
      return {
        json: {
          state: "ready",
          works: BOOKS.filter((b) => b.id !== id)
            .slice(0, 4)
            .map(workSummary),
        },
      };
    }
    const work = /^\/api\/library\/works\/([^/]+)$/.exec(pathname);
    if (method === "GET" && work && work[1] !== "meaning") {
      const detail = workDetail(decodeURIComponent(work[1] ?? ""));
      return detail ? { json: detail } : { status: 404, json: { error: "no such work" } };
    }
  }
  const routes = viewer === "signed-out" ? SIGNED_OUT : signedIn(viewer);
  return routes[`${method} ${pathname}`]?.(body);
}

/**
 * Variations of the sample library, for pages that only appear in some
 * states (a fresh install, plugins to configure, a book playing). Each
 * answers some requests differently and leaves the rest to the defaults.
 */
export const SCENARIOS = {
  /** A fresh install: the operator gets the welcome screen. */
  fresh: {
    "GET /api/config/join": () => ({
      json: {
        fresh: true,
        has_owner: false,
        shared_branch_eligible: true,
        shared_branch_problem: null,
        default_branch_name: "Living room server",
        links: [],
      },
    }),
  },
  /** Things Server Settings asks the operator to look at. */
  attention: {
    "GET /api/config/master-key": () => ({
      json: {
        store: "keychain",
        recovery_copy: "skipped",
        recorded_at: ago(2 * DAY),
        needs_recovery_copy: true,
      },
    }),
    "GET /api/diagnostics/reports": () => ({
      json: {
        reports: [
          {
            report_id: "3f2a9c1e-5b7d-4e8a-9c0f-1a2b3c4d5e6f",
            trigger: "error_burst",
            version: "0.4.0",
            sent_at_unix_ms: NOW_MS - 2 * DAY * 60_000,
            access_code: "MAPLE-RIVER-42",
          },
        ],
      },
    }),
  },
  /** This server reads another library's books. */
  window: {
    "GET /api/config/join/window": () => ({
      json: {
        libraries: [
          {
            library_id: "lib-ana",
            owner_origin: "https://ana.example.net",
            node_id: "node-main",
            pinned: true,
            state: "joined",
            enrolled_at: ago(60 * DAY),
            share_books: true,
            include_branches: true,
            work_for_members: false,
            keep_member_copies: false,
            list_members_books: true,
            keep_copies: false,
            running: true,
          },
        ],
        cards: [],
        books: [
          {
            // Also in this library: shown on its own row.
            key: "work:w-pride",
            work_id: "w-pride",
            editions: [
              {
                library_id: "lib-ana",
                owner_origin: "https://ana.example.net",
                store: "libro",
                marketplace: "us",
                product_id: "p-pride",
                formats: ["audio"],
                title: "Pride and Prejudice",
                authors: "Jane Austen",
                narrators: "Iris Vale",
                length_minutes: 707,
                yours: true,
              },
            ],
          },
          {
            key: "ana/emma",
            work_id: null,
            editions: [
              {
                library_id: "lib-ana",
                owner_origin: "https://ana.example.net",
                store: "libro",
                marketplace: "us",
                product_id: "p-emma",
                formats: ["audio"],
                title: "Emma",
                authors: "Jane Austen",
                narrators: "Iris Vale",
                length_minutes: 960,
                yours: true,
              },
            ],
          },
          {
            key: "ana/kim",
            work_id: null,
            editions: [
              {
                library_id: "lib-ana",
                owner_origin: "https://ana.example.net",
                store: "echo",
                marketplace: "us",
                product_id: "p-kim",
                formats: ["audio", "pdf"],
                title: "Kim",
                authors: "Rudyard Kipling",
                narrators: "Theo Marsh",
                length_minutes: 720,
                yours: false,
                request: { work_id: "w-ana-kim", from: "branch" },
              },
            ],
          },
        ],
      },
    }),
  },
  /** The sign-in page offers single sign-on. */
  sso: {
    "GET /api/auth/signin": () => ({
      json: {
        operator_token: true,
        oidc: [
          { id: "google", name: "Google", preset: "google" },
          { id: "work", name: "Example Co. sign-in", preset: null },
        ],
        integrations: [{ id: "echo", name: "Echo Books" }],
        require_second_factor: false,
      },
    }),
  },
  /** The library requires a second factor the owner has not set up. */
  enroll: {
    "GET /api/auth/me": () => ({
      json: {
        authenticated: true,
        role: "owner",
        default_view: "library",
        can_acquire: true,
        elevated: false,
        administers_nodes: true,
        user: OWNER,
        second_factor: { required: true, totp: false, passkey_count: 0, enrolled: false },
        see_prices: true,
      },
    }),
    "POST /api/auth/totp/enroll/begin": () => ({
      json: {
        secret: "JBSWY3DPEHPK3PXP",
        otpauth_url: "otpauth://totp/Bookclerk:robin?secret=JBSWY3DPEHPK3PXP&issuer=Bookclerk",
        qr_svg: QR_SVG,
      },
    }),
  },
} satisfies Record<string, Routes>;

/** A named variation of the sample library. */
export type Scenario = keyof typeof SCENARIOS;
