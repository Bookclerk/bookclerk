/**
 * Cloudflare dynamic DNS ingress (workerd guest).
 *
 * A direct ingress: it keeps the host name's A and AAAA records on
 * Cloudflare pointed at this computer's public addresses (see
 * `lifecycle.ts`), and traffic comes straight to the daemon's own TLS
 * listener, so TLS ends at home.
 */
import { BookclerkEntrypoint, IngressEntrypoint, ProvisioningEntrypoint, } from "@bookclerk/plugin-sdk/workerd";
import { dnsHealth, liveContext, startDns, stopDns } from "./lifecycle.js";
import { deprovisionDns, provisionDns } from "./provision.js";
// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.
/** `ingress` entrypoint: keeps the records current. */
export class Ingress extends IngressEntrypoint {
    /**
     * Points the records at this computer.
     *
     * @returns The public origin.
     */
    start() {
        return startDns(liveContext(this.env));
    }
    /**
     * Follows an address change and reports whether the records point here.
     *
     * @returns The address's state.
     */
    health() {
        return dnsHealth(liveContext(this.env));
    }
    /**
     * Stops following the addresses; the records stay.
     *
     * @returns Resolves at once.
     */
    stop() {
        stopDns();
        return Promise.resolve();
    }
}
/** `provisioning` entrypoint: the guided setup (see `provision.ts`). */
export class Provisioning extends ProvisioningEntrypoint {
    /**
     * Mints the zone's token from the pasted one.
     *
     * @param request - The operator's grant.
     * @returns The host name and the token to keep.
     */
    provision(request) {
        return provisionDns(liveContext(this.env), request);
    }
    /**
     * Deletes the kept records and the minted token.
     *
     * @param request - The operator's grant, or kind `none`.
     * @returns Resolves once they are gone.
     */
    deprovision(request) {
        return deprovisionDns(liveContext(this.env), request);
    }
}
/** Default entrypoint: nothing beyond the SDK's own handlers. */
export default class CloudflareDnsIngress extends BookclerkEntrypoint {
}
