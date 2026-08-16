/**
 * Convert WebAuthn JSON (base64url) from the daemon into browser ArrayBuffers
 * and serialize assertion/attestation responses back to JSON.
 */

/**
 * @param value
 * @returns
 */
function b64urlToBuf(value: string): ArrayBuffer {
  const pad = "=".repeat((4 - (value.length % 4)) % 4);
  const b64 = (value + pad).replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) {
    bytes[i] = bin.charCodeAt(i);
  }
  return bytes.buffer;
}

function bufToB64url(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) {
    bin += String.fromCharCode(b);
  }
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function isB64urlString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

/**
 * Daemon registration challenge (`CreationChallengeResponse` + `challenge_id`).
 */
export interface PasskeyCeremonyOptions {
  challenge_id: string;
  publicKey: Record<string, unknown>;
}

/**
 * Decode a registration `publicKey` for `navigator.credentials.create`.
 *
 * @param publicKey
 * @returns
 */
export function creationOptionsFromJson(
  publicKey: Record<string, unknown>,
): PublicKeyCredentialCreationOptions {
  const user = (publicKey.user ?? {}) as Record<string, unknown>;
  const exclude = Array.isArray(publicKey.excludeCredentials)
    ? publicKey.excludeCredentials.map((c) => {
        const cred = c as Record<string, unknown>;
        return {
          type: "public-key" as const,
          id: isB64urlString(cred.id) ? b64urlToBuf(cred.id) : (cred.id as BufferSource),
          transports: cred.transports as AuthenticatorTransport[] | undefined,
        };
      })
    : undefined;
  return {
    ...((publicKey as unknown) as PublicKeyCredentialCreationOptions),
    challenge: isB64urlString(publicKey.challenge)
      ? b64urlToBuf(publicKey.challenge)
      : (publicKey.challenge as BufferSource),
    user: {
      name: String(user.name ?? ""),
      displayName: String(user.displayName ?? user.name ?? ""),
      id: isB64urlString(user.id) ? b64urlToBuf(user.id) : (user.id as BufferSource),
    },
    excludeCredentials: exclude,
  };
}

/**
 * Decode an assertion `publicKey` for `navigator.credentials.get`.
 *
 * @param publicKey
 * @returns
 */
export function requestOptionsFromJson(
  publicKey: Record<string, unknown>,
): PublicKeyCredentialRequestOptions {
  const allow = Array.isArray(publicKey.allowCredentials)
    ? publicKey.allowCredentials.map((c) => {
        const cred = c as Record<string, unknown>;
        return {
          type: "public-key" as const,
          id: isB64urlString(cred.id) ? b64urlToBuf(cred.id) : (cred.id as BufferSource),
          transports: cred.transports as AuthenticatorTransport[] | undefined,
        };
      })
    : undefined;
  return {
    ...((publicKey as unknown) as PublicKeyCredentialRequestOptions),
    challenge: isB64urlString(publicKey.challenge)
      ? b64urlToBuf(publicKey.challenge)
      : (publicKey.challenge as BufferSource),
    allowCredentials: allow,
  };
}

/**
 * Serialize a WebAuthn credential for the daemon finish endpoints.
 *
 * @param cred
 * @returns
 */
export function credentialToJson(cred: PublicKeyCredential): Record<string, unknown> {
  const response = cred.response;
  const out: Record<string, unknown> = {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    clientExtensionResults: cred.getClientExtensionResults(),
  };
  if (response instanceof AuthenticatorAttestationResponse) {
    out.response = {
      clientDataJSON: bufToB64url(response.clientDataJSON),
      attestationObject: bufToB64url(response.attestationObject),
      transports:
        typeof response.getTransports === "function" ? response.getTransports() : undefined,
    };
  } else if (response instanceof AuthenticatorAssertionResponse) {
    out.response = {
      clientDataJSON: bufToB64url(response.clientDataJSON),
      authenticatorData: bufToB64url(response.authenticatorData),
      signature: bufToB64url(response.signature),
      userHandle: response.userHandle ? bufToB64url(response.userHandle) : null,
    };
  }
  return out;
}

/**
 * How long to wait for a WebAuthn prompt. Browsers without passkey support
 * (or in-app WebViews that stub the API) can hang forever on
 * `navigator.credentials.get` / `.create`.
 */
export const PASSKEY_PROMPT_TIMEOUT_MS = 45_000;

const PASSKEYS_UNSUPPORTED =
  "This browser does not support passkeys. Use a password instead.";

const PASSKEY_TIMED_OUT =
  "Passkey request timed out. This browser may not support passkeys — use a password instead.";

/**
 * True when the WebAuthn credential API is present.
 *
 * @returns Whether `PublicKeyCredential` and credential create/get are available.
 */
export function passkeysSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential === "function" &&
    typeof navigator.credentials?.create === "function" &&
    typeof navigator.credentials?.get === "function"
  );
}

function mapPasskeyError(err: unknown, cancelled: string): Error {
  if (err instanceof DOMException) {
    switch (err.name) {
      case "AbortError":
        return new Error(PASSKEY_TIMED_OUT);
      case "NotSupportedError":
        return new Error(PASSKEYS_UNSUPPORTED);
      case "NotAllowedError":
        return new Error(cancelled);
      case "InvalidStateError":
        return new Error("This passkey is already registered on this host.");
      default:
        break;
    }
  }
  if (err instanceof Error) return err;
  return new Error(cancelled);
}

async function withPasskeyTimeout(
  run: (signal: AbortSignal) => Promise<PublicKeyCredential | null>,
  cancelled: string,
): Promise<PublicKeyCredential> {
  if (!passkeysSupported()) {
    throw new Error(PASSKEYS_UNSUPPORTED);
  }
  const controller = new AbortController();
  let timedOut = false;
  try {
    const cred = await new Promise<PublicKeyCredential | null>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        timedOut = true;
        controller.abort();
        reject(new DOMException("The operation timed out.", "AbortError"));
      }, PASSKEY_PROMPT_TIMEOUT_MS);
      void run(controller.signal).then(
        (value) => {
          window.clearTimeout(timer);
          resolve(value);
        },
        (err: unknown) => {
          window.clearTimeout(timer);
          reject(err);
        },
      );
    });
    if (!cred) throw new Error(cancelled);
    return cred;
  } catch (err) {
    if (timedOut) throw new Error(PASSKEY_TIMED_OUT);
    throw mapPasskeyError(err, cancelled);
  }
}

/**
 * Run a registration ceremony and return JSON for `/register/finish`.
 *
 * @param options
 * @returns
 */
export async function createPasskey(
  options: PasskeyCeremonyOptions,
): Promise<{ challenge_id: string; credential: Record<string, unknown> }> {
  const cred = await withPasskeyTimeout(
    (signal) =>
      navigator.credentials.create({
        publicKey: {
          ...creationOptionsFromJson(options.publicKey),
          timeout: PASSKEY_PROMPT_TIMEOUT_MS,
        },
        signal,
      }) as Promise<PublicKeyCredential | null>,
    "Passkey registration was cancelled.",
  );
  return { challenge_id: options.challenge_id, credential: credentialToJson(cred) };
}

/**
 * Run an assertion ceremony and return JSON for login/elevate finish.
 *
 * @param options
 * @returns
 */
export async function assertPasskey(
  options: PasskeyCeremonyOptions,
): Promise<{ challenge_id: string; credential: Record<string, unknown> }> {
  const cred = await withPasskeyTimeout(
    (signal) =>
      navigator.credentials.get({
        publicKey: {
          ...requestOptionsFromJson(options.publicKey),
          timeout: PASSKEY_PROMPT_TIMEOUT_MS,
        },
        signal,
      }) as Promise<PublicKeyCredential | null>,
    "Passkey sign-in was cancelled.",
  );
  return { challenge_id: options.challenge_id, credential: credentialToJson(cred) };
}
