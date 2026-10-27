/** Browser side of passkeys: the server's JSON options in, the browser's
 *  credential out as JSON (base64url binary fields, as webauthn-rs expects). */

type Json = Record<string, unknown>;

function fromB64url(s: string): ArrayBuffer {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function toB64url(buf: ArrayBuffer | null | undefined): string | undefined {
  if (!buf) return undefined;
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function decodeIds<T extends { id: string }>(list: T[] | undefined) {
  return list?.map((c) => ({ ...c, id: fromB64url(c.id) }));
}

export function passkeysSupported(): boolean {
  return typeof window !== "undefined" && !!window.PublicKeyCredential && !!navigator.credentials;
}

/** Whether passkeys registered for `rpId` work from the page's current address. */
export function rpMatchesHere(rpId: string | null): boolean {
  if (!rpId) return false;
  const host = location.hostname;
  return host === rpId || host.endsWith(`.${rpId}`);
}

export async function createPasskey(options: { publicKey: Json }): Promise<Json> {
  const pk = options.publicKey as Json & {
    challenge: string;
    user: Json & { id: string };
    excludeCredentials?: { id: string; type: string }[];
  };
  const publicKey = {
    ...pk,
    challenge: fromB64url(pk.challenge),
    user: { ...pk.user, id: fromB64url(pk.user.id) },
    excludeCredentials: decodeIds(pk.excludeCredentials),
  } as unknown as PublicKeyCredentialCreationOptions;
  const cred = (await navigator.credentials.create({ publicKey })) as PublicKeyCredential | null;
  if (!cred) throw new Error("No passkey was created.");
  const res = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      clientDataJSON: toB64url(res.clientDataJSON),
      attestationObject: toB64url(res.attestationObject),
      transports: res.getTransports?.(),
    },
    clientExtensionResults: cred.getClientExtensionResults(),
  };
}

export async function getPasskey(options: { publicKey: Json }): Promise<Json> {
  const pk = options.publicKey as Json & {
    challenge: string;
    allowCredentials?: { id: string; type: string }[];
  };
  const publicKey = {
    ...pk,
    challenge: fromB64url(pk.challenge),
    allowCredentials: decodeIds(pk.allowCredentials),
  } as unknown as PublicKeyCredentialRequestOptions;
  const cred = (await navigator.credentials.get({ publicKey })) as PublicKeyCredential | null;
  if (!cred) throw new Error("No passkey was used.");
  const res = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: toB64url(res.authenticatorData),
      clientDataJSON: toB64url(res.clientDataJSON),
      signature: toB64url(res.signature),
      userHandle: toB64url(res.userHandle),
    },
    clientExtensionResults: cred.getClientExtensionResults(),
  };
}

/** A user-facing message for a failed or cancelled browser prompt. */
export function passkeyError(e: unknown): string {
  if (e instanceof DOMException) {
    if (e.name === "NotAllowedError") return "The passkey prompt was closed or timed out.";
    if (e.name === "InvalidStateError") return "This device already has a passkey for your account.";
    if (e.name === "SecurityError") return "Passkeys don't work at this address.";
  }
  return e instanceof Error ? e.message : "The passkey didn't work.";
}
