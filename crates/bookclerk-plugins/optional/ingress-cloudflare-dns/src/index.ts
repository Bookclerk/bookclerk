/**
 * Cloudflare dynamic DNS ingress (workerd guest).
 *
 * A direct ingress: it keeps the host name's A and AAAA records on
 * Cloudflare pointed at this computer's public addresses (see
 * `lifecycle.ts`), and traffic comes straight to the daemon's own TLS
 * listener, so TLS ends at home.
 */

import {
  BookclerkEntrypoint,
  IngressEntrypoint,
  type IngressHealth,
  type IngressStarted,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { dnsHealth, liveContext, startDns, stopDns } from "./lifecycle.ts";

// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.

/** `ingress` entrypoint: keeps the records current. */
export class Ingress extends IngressEntrypoint<Env> {
  /**
   * Points the records at this computer.
   *
   * @returns The public origin.
   */
  override start(): Promise<IngressStarted> {
    return startDns(liveContext(this.env));
  }

  /**
   * Follows an address change and reports whether the records point here.
   *
   * @returns The address's state.
   */
  override health(): Promise<IngressHealth> {
    return dnsHealth(liveContext(this.env));
  }

  /**
   * Stops following the addresses; the records stay.
   *
   * @returns Resolves at once.
   */
  override stop(): Promise<void> {
    stopDns();
    return Promise.resolve();
  }
}

/** Default entrypoint: nothing beyond the SDK's own handlers. */
export default class CloudflareDnsIngress extends BookclerkEntrypoint<Env> {}
