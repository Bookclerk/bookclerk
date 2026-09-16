/**
 * GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
 *
 * Unpacked single-segment Cap'n Proto codecs for every struct and method
 * envelope in `plugin.capnp`. Layout numbers come from
 * `schema/plugin.layout.json` (Cap'n Proto compiler output), not from
 * hand-derived tables. Pointer targets are allocated in declaration order,
 * depth first, which matches the `capnpc-rust` builder so Rust, TypeScript,
 * and Python produce byte-identical messages for the same value.
 *
 * @internal
 * @module
 */

import * as A from "./abi.js";
import type * as T from "./generated.js";
import {
  CapnpMessage,
  CapnpReader,
  NO_CAPS,
  type CapTable,
  type CapnpStruct,
  type StructReader,
} from "./db-capnp.js";

/**
 * Struct codec over the runtime Cap'n builders/readers.
 *
 * @internal
 */
export interface StructCodec<V> {
  /** Data section size in 64-bit words. */
  readonly dataWords: number;
  /** Pointer section size in pointers. */
  readonly pointerCount: number;
  /** Writes `value` into an allocated struct of this codec's size. */
  write(s: CapnpStruct, value: V, caps: CapTable): void;
  /** Reads a value from a struct at least this codec's size. */
  read(s: StructReader, caps: CapTable): V;
}

/**
 * Encodes `value` as a standalone unpacked Cap'n message.
 *
 * @param codec - Root struct codec.
 * @param value - Value to encode.
 * @param caps - Transport capability table (required only for interface-typed fields).
 * @returns Unpacked Cap'n stream bytes.
 * @internal
 */
export function encodeMessage<V>(codec: StructCodec<V>, value: V, caps: CapTable = NO_CAPS): Uint8Array {
  const msg = new CapnpMessage();
  codec.write(msg.initRoot(codec.dataWords, codec.pointerCount), value, caps);
  return msg.finish();
}

/**
 * Decodes a standalone unpacked Cap'n message.
 *
 * @param codec - Root struct codec.
 * @param bytes - Unpacked Cap'n stream.
 * @param caps - Transport capability table (required only for interface-typed fields).
 * @returns Decoded value.
 * @internal
 */
export function decodeMessage<V>(codec: StructCodec<V>, bytes: Uint8Array, caps: CapTable = NO_CAPS): V {
  return codec.read(new CapnpReader(bytes).root(codec.dataWords, codec.pointerCount), caps);
}

function ord(table: readonly string[], name: string, enumName: string): number {
  const i = table.indexOf(name);
  if (i < 0) {
    throw new Error(`unknown ${enumName} value: ${name}`);
  }
  return i;
}

function fromOrd<S extends string>(table: readonly S[], ordinal: number, enumName: string): S {
  const v = table[ordinal];
  if (v === undefined) {
    throw new Error(`unknown ${enumName} ordinal: ${ordinal}`);
  }
  return v;
}

function unknownUnion(struct: string, member: string | number): Error {
  return new Error(`unknown ${struct} union member: ${String(member)}`);
}

/** Default for absent `Data` fields (Cap'n Proto empty blob). */
const EMPTY_BYTES = new Uint8Array(0);

/**
 * Params envelope of `ByteSource.pull`.
 *
 * @internal
 */
export interface ByteSourcePullParams {
  maxBytes: number;
}

/**
 * Results envelope of `ByteSource.pull`.
 *
 * @internal
 */
export interface ByteSourcePullResults {
  result: T.PullReply;
}

/**
 * Params envelope of `Destination.head`.
 *
 * @internal
 */
export interface DestinationHeadParams {
  key: string;
}

/**
 * Results envelope of `Destination.head`.
 *
 * @internal
 */
export interface DestinationHeadResults {
  result: T.HeadReply;
}

/**
 * Params envelope of `Destination.list`.
 *
 * @internal
 */
export interface DestinationListParams {
  options: T.ListOptions;
}

/**
 * Results envelope of `Destination.list`.
 *
 * @internal
 */
export interface DestinationListResults {
  result: T.ListReply;
}

/**
 * Params envelope of `Destination.get`.
 *
 * @internal
 */
export interface DestinationGetParams {
  key: string;
  options: T.ReadOptions;
}

/**
 * Results envelope of `Destination.get`.
 *
 * @internal
 */
export interface DestinationGetResults {
  result: T.GetReply;
}

/**
 * Params envelope of `Destination.put`.
 *
 * @internal
 */
export interface DestinationPutParams {
  key: string;
  body: T.ByteSource;
  options: T.WriteOptions;
}

/**
 * Results envelope of `Destination.put`.
 *
 * @internal
 */
export interface DestinationPutResults {
  result: T.PutReply;
}

/**
 * Params envelope of `Destination.copy`.
 *
 * @internal
 */
export interface DestinationCopyParams {
  from: string;
  to: string;
}

/**
 * Results envelope of `Destination.copy`.
 *
 * @internal
 */
export interface DestinationCopyResults {
  result: T.CopyReply;
}

/**
 * Params envelope of `Destination.delete`.
 *
 * @internal
 */
export interface DestinationDeleteParams {
  key: string;
}

/**
 * Results envelope of `Destination.delete`.
 *
 * @internal
 */
export interface DestinationDeleteResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `Destination.commit`.
 *
 * @internal
 */
export interface DestinationCommitParams {
  key: string;
  commitToken: string;
}

/**
 * Results envelope of `Destination.commit`.
 *
 * @internal
 */
export interface DestinationCommitResults {
  result: T.PutReply;
}

/**
 * Params envelope of `Destination.abortStage`.
 *
 * @internal
 */
export interface DestinationAbortStageParams {
  key: string;
  commitToken: string;
}

/**
 * Results envelope of `Destination.abortStage`.
 *
 * @internal
 */
export interface DestinationAbortStageResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `Source.open`.
 *
 * @internal
 */
export interface SourceOpenParams {
  key: string;
}

/**
 * Results envelope of `Source.open`.
 *
 * @internal
 */
export interface SourceOpenResults {
  result: T.OpenReply;
}

/**
 * Params envelope of `ProgressSink.report`.
 *
 * @internal
 */
export interface ProgressSinkReportParams {
  percent: number;
  message: string;
}

/**
 * Results envelope of `ProgressSink.report`.
 *
 * @internal
 */
export interface ProgressSinkReportResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `Cancellation.poll`.
 *
 * @internal
 */
export interface CancellationPollParams {
}

/**
 * Results envelope of `Cancellation.poll`.
 *
 * @internal
 */
export interface CancellationPollResults {
  cancelled: boolean;
}

/**
 * Params envelope of `JobRunner.job`.
 *
 * @internal
 */
export interface JobRunnerJobParams {
  controller: T.JobController;
}

/**
 * Results envelope of `JobRunner.job`.
 *
 * @internal
 */
export interface JobRunnerJobResults {
  result: T.HandleReply;
}

/**
 * Params envelope of `EventConsumer.event`.
 *
 * @internal
 */
export interface EventConsumerEventParams {
  batch: T.EventBatch;
}

/**
 * Results envelope of `EventConsumer.event`.
 *
 * @internal
 */
export interface EventConsumerEventResults {
  result: T.EventBatchReply;
}

/**
 * Params envelope of `EventPublisher.publish`.
 *
 * @internal
 */
export interface EventPublisherPublishParams {
  event: T.PluginEvent;
}

/**
 * Results envelope of `EventPublisher.publish`.
 *
 * @internal
 */
export interface EventPublisherPublishResults {
  result: T.PublishReply;
}

/**
 * Params envelope of `ContentSource.login`.
 *
 * @internal
 */
export interface ContentSourceLoginParams {
  params: T.LoginParams;
}

/**
 * Results envelope of `ContentSource.login`.
 *
 * @internal
 */
export interface ContentSourceLoginResults {
  result: T.LoginReply;
}

/**
 * Params envelope of `ContentSource.scan`.
 *
 * @internal
 */
export interface ContentSourceScanParams {
  params: T.ScanParams;
}

/**
 * Results envelope of `ContentSource.scan`.
 *
 * @internal
 */
export interface ContentSourceScanResults {
  result: T.ScanReply;
}

/**
 * Params envelope of `ContentSource.fetchTitle`.
 *
 * @internal
 */
export interface ContentSourceFetchTitleParams {
  params: T.FetchTitleParams;
}

/**
 * Results envelope of `ContentSource.fetchTitle`.
 *
 * @internal
 */
export interface ContentSourceFetchTitleResults {
  result: T.FetchTitleReply;
}

/**
 * Params envelope of `ContentSource.listAccounts`.
 *
 * @internal
 */
export interface ContentSourceListAccountsParams {
}

/**
 * Results envelope of `ContentSource.listAccounts`.
 *
 * @internal
 */
export interface ContentSourceListAccountsResults {
  result: T.SourceAccountsReply;
}

/**
 * Params envelope of `ContentSource.loginStart`.
 *
 * @internal
 */
export interface ContentSourceLoginStartParams {
  params: T.LoginParams;
}

/**
 * Results envelope of `ContentSource.loginStart`.
 *
 * @internal
 */
export interface ContentSourceLoginStartResults {
  result: T.LoginStartReply;
}

/**
 * Params envelope of `ContentSource.loginComplete`.
 *
 * @internal
 */
export interface ContentSourceLoginCompleteParams {
  params: T.LoginCompleteParams;
}

/**
 * Results envelope of `ContentSource.loginComplete`.
 *
 * @internal
 */
export interface ContentSourceLoginCompleteResults {
  result: T.LoginReply;
}

/**
 * Params envelope of `ContentSource.searchCatalog`.
 *
 * @internal
 */
export interface ContentSourceSearchCatalogParams {
  params: T.SearchCatalogParams;
}

/**
 * Results envelope of `ContentSource.searchCatalog`.
 *
 * @internal
 */
export interface ContentSourceSearchCatalogResults {
  result: T.CatalogHitsReply;
}

/**
 * Params envelope of `ContentSource.expandCandidates`.
 *
 * @internal
 */
export interface ContentSourceExpandCandidatesParams {
  params: T.ExpandCandidatesParams;
}

/**
 * Results envelope of `ContentSource.expandCandidates`.
 *
 * @internal
 */
export interface ContentSourceExpandCandidatesResults {
  result: T.CatalogHitsReply;
}

/**
 * Params envelope of `ContentSource.purchaseHint`.
 *
 * @internal
 */
export interface ContentSourcePurchaseHintParams {
  params: T.PurchaseHintParams;
}

/**
 * Results envelope of `ContentSource.purchaseHint`.
 *
 * @internal
 */
export interface ContentSourcePurchaseHintResults {
  result: T.PurchaseHintReply;
}

/**
 * Params envelope of `ContentSource.listDeals`.
 *
 * @internal
 */
export interface ContentSourceListDealsParams {
  params: T.ListDealsParams;
}

/**
 * Results envelope of `ContentSource.listDeals`.
 *
 * @internal
 */
export interface ContentSourceListDealsResults {
  result: T.CatalogHitsReply;
}

/**
 * Params envelope of `ContentSource.health`.
 *
 * @internal
 */
export interface ContentSourceHealthParams {
}

/**
 * Results envelope of `ContentSource.health`.
 *
 * @internal
 */
export interface ContentSourceHealthResults {
  result: T.HealthReply;
}

/**
 * Params envelope of `ContentSource.diagnose`.
 *
 * @internal
 */
export interface ContentSourceDiagnoseParams {
}

/**
 * Results envelope of `ContentSource.diagnose`.
 *
 * @internal
 */
export interface ContentSourceDiagnoseResults {
  result: T.DiagnoseReply;
}

/**
 * Params envelope of `ContentSource.catalogDetail`.
 *
 * @internal
 */
export interface ContentSourceCatalogDetailParams {
  params: T.CatalogDetailParams;
}

/**
 * Results envelope of `ContentSource.catalogDetail`.
 *
 * @internal
 */
export interface ContentSourceCatalogDetailResults {
  result: T.CatalogDetailReply;
}

/**
 * Params envelope of `RemoteLibrary.health`.
 *
 * @internal
 */
export interface RemoteLibraryHealthParams {
}

/**
 * Results envelope of `RemoteLibrary.health`.
 *
 * @internal
 */
export interface RemoteLibraryHealthResults {
  result: T.HealthReply;
}

/**
 * Params envelope of `RemoteLibrary.start`.
 *
 * @internal
 */
export interface RemoteLibraryStartParams {
}

/**
 * Results envelope of `RemoteLibrary.start`.
 *
 * @internal
 */
export interface RemoteLibraryStartResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `RemoteLibrary.stop`.
 *
 * @internal
 */
export interface RemoteLibraryStopParams {
}

/**
 * Results envelope of `RemoteLibrary.stop`.
 *
 * @internal
 */
export interface RemoteLibraryStopResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `RemoteLibrary.diagnose`.
 *
 * @internal
 */
export interface RemoteLibraryDiagnoseParams {
}

/**
 * Results envelope of `RemoteLibrary.diagnose`.
 *
 * @internal
 */
export interface RemoteLibraryDiagnoseResults {
  result: T.DiagnoseReply;
}

/**
 * Params envelope of `RemoteLibrary.scanLibrary`.
 *
 * @internal
 */
export interface RemoteLibraryScanLibraryParams {
  params: T.ScanLibraryParams;
}

/**
 * Results envelope of `RemoteLibrary.scanLibrary`.
 *
 * @internal
 */
export interface RemoteLibraryScanLibraryResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `RemoteLibrary.syncListening`.
 *
 * @internal
 */
export interface RemoteLibrarySyncListeningParams {
}

/**
 * Results envelope of `RemoteLibrary.syncListening`.
 *
 * @internal
 */
export interface RemoteLibrarySyncListeningResults {
  result: T.SyncListeningReply;
}

/**
 * Params envelope of `RemoteLibrary.pollEvents`.
 *
 * @internal
 */
export interface RemoteLibraryPollEventsParams {
}

/**
 * Results envelope of `RemoteLibrary.pollEvents`.
 *
 * @internal
 */
export interface RemoteLibraryPollEventsResults {
  result: T.EventPollReply;
}

/**
 * Params envelope of `PluginCli.describe`.
 *
 * @internal
 */
export interface PluginCliDescribeParams {
}

/**
 * Results envelope of `PluginCli.describe`.
 *
 * @internal
 */
export interface PluginCliDescribeResults {
  result: T.CliSchemaReply;
}

/**
 * Params envelope of `PluginCli.invoke`.
 *
 * @internal
 */
export interface PluginCliInvokeParams {
  params: T.CliInvokeParams;
}

/**
 * Results envelope of `PluginCli.invoke`.
 *
 * @internal
 */
export interface PluginCliInvokeResults {
  result: T.CliInvokeReply;
}

/**
 * Params envelope of `Oidc.clients`.
 *
 * @internal
 */
export interface OidcClientsParams {
}

/**
 * Results envelope of `Oidc.clients`.
 *
 * @internal
 */
export interface OidcClientsResults {
  result: T.OidcClientsReply;
}

/**
 * Params envelope of `Oidc.authenticateUser`.
 *
 * @internal
 */
export interface OidcAuthenticateUserParams {
  params: T.AuthenticateUserParams;
}

/**
 * Results envelope of `Oidc.authenticateUser`.
 *
 * @internal
 */
export interface OidcAuthenticateUserResults {
  result: T.ExternalUserReply;
}

/**
 * Params envelope of `Database.openSession`.
 *
 * @internal
 */
export interface DatabaseOpenSessionParams {
}

/**
 * Results envelope of `Database.openSession`.
 *
 * @internal
 */
export interface DatabaseOpenSessionResults {
  result: T.AdapterSessionReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.capabilities`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionCapabilitiesParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.capabilities`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionCapabilitiesResults {
  result: T.DbCapabilitiesReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.execute`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionExecuteParams {
  request: T.AdapterExecuteRequest;
}

/**
 * Results envelope of `AdapterDatabaseSession.execute`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionExecuteResults {
  result: T.ExecuteResultReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.close`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionCloseParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.close`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionCloseResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.bootstrap`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionBootstrapParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.bootstrap`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionBootstrapResults {
  result: T.DbBootstrapReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.exportIdentity`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionExportIdentityParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.exportIdentity`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionExportIdentityResults {
  result: T.IdentityExportReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.importIdentity`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionImportIdentityParams {
  rows: T.IdentityHighWater[];
}

/**
 * Results envelope of `AdapterDatabaseSession.importIdentity`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionImportIdentityResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.listUserRelations`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionListUserRelationsParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.listUserRelations`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionListUserRelationsResults {
  result: T.UserRelationsReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.prepareUnitRestore`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionPrepareUnitRestoreParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.prepareUnitRestore`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionPrepareUnitRestoreResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.dropUserRelations`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionDropUserRelationsParams {
  names: string[];
}

/**
 * Results envelope of `AdapterDatabaseSession.dropUserRelations`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionDropUserRelationsResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `AdapterDatabaseSession.assertRestoreConstraints`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionAssertRestoreConstraintsParams {
}

/**
 * Results envelope of `AdapterDatabaseSession.assertRestoreConstraints`.
 *
 * @internal
 */
export interface AdapterDatabaseSessionAssertRestoreConstraintsResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `GuestDatabase.execute`.
 *
 * @internal
 */
export interface GuestDatabaseExecuteParams {
  request: T.ExecuteRequest;
}

/**
 * Results envelope of `GuestDatabase.execute`.
 *
 * @internal
 */
export interface GuestDatabaseExecuteResults {
  result: T.ExecuteResultReply;
}

/**
 * Params envelope of `GuestDatabase.close`.
 *
 * @internal
 */
export interface GuestDatabaseCloseParams {
}

/**
 * Results envelope of `GuestDatabase.close`.
 *
 * @internal
 */
export interface GuestDatabaseCloseResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `PluginWorker.describe`.
 *
 * @internal
 */
export interface PluginWorkerDescribeParams {
}

/**
 * Results envelope of `PluginWorker.describe`.
 *
 * @internal
 */
export interface PluginWorkerDescribeResults {
  result: T.DescribeReply;
}

/**
 * Params envelope of `PluginWorker.open`.
 *
 * @internal
 */
export interface PluginWorkerOpenParams {
  invocation: T.Invocation;
  bindings: T.Bindings;
}

/**
 * Results envelope of `PluginWorker.open`.
 *
 * @internal
 */
export interface PluginWorkerOpenResults {
  result: T.EntrypointsReply;
}

/**
 * Params envelope of `PluginWorker.shutdown`.
 *
 * @internal
 */
export interface PluginWorkerShutdownParams {
}

/**
 * Results envelope of `PluginWorker.shutdown`.
 *
 * @internal
 */
export interface PluginWorkerShutdownResults {
  result: T.EmptyReply;
}

/**
 * Params envelope of `PluginWorker.databaseMigrations`.
 *
 * @internal
 */
export interface PluginWorkerDatabaseMigrationsParams {
  binding: string;
}

/**
 * Results envelope of `PluginWorker.databaseMigrations`.
 *
 * @internal
 */
export interface PluginWorkerDatabaseMigrationsResults {
  result: T.PluginMigrationsReply;
}

/**
 * Wire codec for `ScalarLimits` (2 data words, 0 pointers).
 *
 * @internal
 */
export const ScalarLimitsCodec: StructCodec<T.ScalarLimits> = {
  dataWords: 2,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.maxScalarBytes ?? 0);
    s.setUint32(1, v.maxStreamWindowBytes ?? 0);
    s.setUint32(2, v.maxListPage ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      maxScalarBytes: s.getUint32(0),
      maxStreamWindowBytes: s.getUint32(1),
      maxListPage: s.getUint32(2),
    };
  },
};

/**
 * Wire codec for `PluginError` (0 data words, 2 pointers).
 *
 * @internal
 */
export const PluginErrorCodec: StructCodec<T.PluginError> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.code ?? "");
    s.setText(1, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      code: s.getText(0),
      message: s.getText(1),
    };
  },
};

/**
 * Wire codec for `ObjectMetadata` (1 data words, 4 pointers).
 *
 * @internal
 */
export const ObjectMetadataCodec: StructCodec<T.ObjectMetadata> = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
    s.setText(1, v.contentType ?? "");
    s.setText(2, v.etag ?? "");
    s.setData(3, v.sha256 ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0)),
      contentType: s.getText(1),
      etag: s.getText(2),
      sha256: s.getData(3),
    };
  },
};

/**
 * Wire codec for `ObjectInfo` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ObjectInfoCodec: StructCodec<T.ObjectInfo> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `ListOptions` (1 data words, 2 pointers).
 *
 * @internal
 */
export const ListOptionsCodec: StructCodec<T.ListOptions> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.prefix ?? "");
    s.setText(1, v.cursor ?? "");
    s.setUint32(0, v.limit ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      prefix: s.getText(0),
      cursor: s.getText(1),
      limit: s.getUint32(0),
    };
  },
};

/**
 * Wire codec for `ListPage` (0 data words, 2 pointers).
 *
 * @internal
 */
export const ListPageCodec: StructCodec<T.ListPage> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.objects ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        ObjectInfoCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setText(1, v.nextCursor ?? "");
  },
  read(s, caps) {
    return {
      objects: s.getStructList(0, 1, 1).map((item) => ObjectInfoCodec.read(item, caps)),
      nextCursor: s.getText(1),
    };
  },
};

/**
 * Wire codec for `ByteRange` (2 data words, 0 pointers).
 *
 * @internal
 */
export const ByteRangeCodec: StructCodec<T.ByteRange> = {
  dataWords: 2,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.offset ?? 0));
    s.setUint64(1, BigInt(v.length ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      offset: Number(s.getUint64(0)),
      length: Number(s.getUint64(1)),
    };
  },
};

/**
 * Wire codec for `ReadOptions` (0 data words, 1 pointers).
 *
 * @internal
 */
export const ReadOptionsCodec: StructCodec<T.ReadOptions> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.range != null) {
      ByteRangeCodec.write(s.initStruct(0, 2, 0), v.range, caps);
    }
  },
  read(s, caps) {
    return {
      range: ByteRangeCodec.read(s.getStruct(0, 2, 0), caps),
    };
  },
};

/**
 * Wire codec for `WriteOptions` (2 data words, 3 pointers).
 *
 * @internal
 */
export const WriteOptionsCodec: StructCodec<T.WriteOptions> = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.contentType ?? "");
    s.setUint64(0, BigInt(v.contentLength ?? 0));
    s.setData(1, v.sha256 ?? EMPTY_BYTES);
    s.setText(2, v.commitToken ?? "");
    s.setBool(64, v.stageOnly ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      contentType: s.getText(0),
      contentLength: Number(s.getUint64(0)),
      sha256: s.getData(1),
      commitToken: s.getText(2),
      stageOnly: s.getBool(64),
    };
  },
};

/**
 * Wire codec for `PutResult` (1 data words, 3 pointers).
 *
 * @internal
 */
export const PutResultCodec: StructCodec<T.PutResult> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.bytesWritten ?? 0));
    s.setText(1, v.etag ?? "");
    s.setData(2, v.sha256 ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      bytesWritten: Number(s.getUint64(0)),
      etag: s.getText(1),
      sha256: s.getData(2),
    };
  },
};

/**
 * Wire codec for `CopyResult` (1 data words, 0 pointers).
 *
 * @internal
 */
export const CopyResultCodec: StructCodec<T.CopyResult> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.bytesCopied ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      bytesCopied: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `PluginDescribe` (2 data words, 10 pointers).
 *
 * @internal
 */
export const PluginDescribeCodec: StructCodec<T.PluginDescribe> = {
  dataWords: 2,
  pointerCount: 10,
  write(s, v, caps) {
    s.setUint32(0, v.apiVersion ?? 0);
    s.setText(0, v.id ?? "");
    s.setText(1, v.displayName ?? "");
    s.setTextList(2, v.rpcFeatures ?? []);
    if (v.scalarLimits != null) {
      ScalarLimitsCodec.write(s.initStruct(3, 2, 0), v.scalarLimits, caps);
    }
    if (v.capabilities != null) {
      PluginCapabilitiesCodec.write(s.initStruct(4, 0, 6), v.capabilities, caps);
    }
    s.setUint16(2, ord(A.PORTAL_AUTH_MODES, v.portalAuthMode ?? A.PORTAL_AUTH_MODES[0]!, "PortalAuthMode"));
    if (v.passwordEnvVar !== undefined) {
      s.setText(5, v.passwordEnvVar ?? "");
    }
    s.setTextList(6, v.aliases ?? []);
    s.setUint32(2, v.sortKey ?? 0);
    if (v.brand != null) {
      BrandCodec.write(s.initStruct(7, 0, 6), v.brand, caps);
    }
    {
      const list = v.configOptions ?? [];
      const items = s.initStructList(8, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        ConfigOptionCodec.write(items[i]!, list[i]!, caps);
      }
    }
    if (v.cli != null) {
      CliSchemaCodec.write(s.initStruct(9, 0, 1), v.cli, caps);
    }
  },
  read(s, caps) {
    const out: T.PluginDescribe = {
      apiVersion: s.getUint32(0),
      id: s.getText(0),
      displayName: s.getText(1),
      rpcFeatures: s.getTextList(2),
      scalarLimits: ScalarLimitsCodec.read(s.getStruct(3, 2, 0), caps),
      capabilities: PluginCapabilitiesCodec.read(s.getStruct(4, 0, 6), caps),
      portalAuthMode: fromOrd(A.PORTAL_AUTH_MODES, s.getUint16(2), "PortalAuthMode"),
      aliases: s.getTextList(6),
      sortKey: s.getUint32(2),
      brand: BrandCodec.read(s.getStruct(7, 0, 6), caps),
      configOptions: s.getStructList(8, 0, 3).map((item) => ConfigOptionCodec.read(item, caps)),
      cli: CliSchemaCodec.read(s.getStruct(9, 0, 1), caps),
    };
    const passwordEnvVarValue = s.getText(5);
    if (!(passwordEnvVarValue === "")) {
      out.passwordEnvVar = passwordEnvVarValue;
    }
    return out;
  },
};

/**
 * Wire codec for `OidcClientTemplate` (1 data words, 5 pointers).
 *
 * @internal
 */
export const OidcClientTemplateCodec: StructCodec<T.OidcClientTemplate> = {
  dataWords: 1,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.clientId ?? "");
    s.setText(1, v.displayName ?? "");
    s.setText(2, v.callbackPath ?? "");
    s.setBool(0, v.publicClient ?? false);
    s.setTextList(3, v.defaultScopes ?? []);
    s.setBool(1, v.issueRefreshToken ?? false);
    s.setText(4, v.originConfigKey ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      clientId: s.getText(0),
      displayName: s.getText(1),
      callbackPath: s.getText(2),
      publicClient: s.getBool(0),
      defaultScopes: s.getTextList(3),
      issueRefreshToken: s.getBool(1),
      originConfigKey: s.getText(4),
    };
  },
};

/**
 * Wire codec for `OidcClientsOk` (0 data words, 1 pointers).
 *
 * @internal
 */
export const OidcClientsOkCodec: StructCodec<T.OidcClientsOk> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.clients ?? [];
      const items = s.initStructList(0, list.length, 1, 5);
      for (let i = 0; i < items.length; i++) {
        OidcClientTemplateCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      clients: s.getStructList(0, 1, 5).map((item) => OidcClientTemplateCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `OidcClientsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const OidcClientsReplyCodec: StructCodec<T.OidcClientsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          OidcClientsOkCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("OidcClientsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: OidcClientsOkCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("OidcClientsReply", disc);
    }
  },
};

/**
 * Wire codec for `ExtensibleConfig` (1 data words, 2 pointers).
 *
 * @internal
 */
export const ExtensibleConfigCodec: StructCodec<T.ExtensibleConfig> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.schemaVersion ?? 0);
    s.setText(0, v.mediaType ?? "");
    s.setData(1, v.payload ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      schemaVersion: s.getUint32(0),
      mediaType: s.getText(0),
      payload: s.getData(1),
    };
  },
};

/**
 * Wire codec for `Bindings` (0 data words, 7 pointers).
 *
 * @internal
 */
export const BindingsCodec: StructCodec<T.Bindings> = {
  dataWords: 0,
  pointerCount: 7,
  write(s, v, caps) {
    if (v.config != null) {
      ExtensibleConfigCodec.write(s.initStruct(0, 1, 2), v.config, caps);
    }
    if (v.secrets != null) {
      ExtensibleConfigCodec.write(s.initStruct(1, 1, 2), v.secrets, caps);
    }
    if (v.adapter != null) {
      DatabaseAdapterConfigCodec.write(s.initStruct(2, 1, 4), v.adapter, caps);
    }
    s.setCap(3, caps.exportCap(v.events));
    {
      const list = v.databases ?? [];
      const items = s.initStructList(4, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        NamedDatabaseCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setCap(5, caps.exportCap(v.cancel));
    s.setCap(6, caps.exportCap(v.storage));
  },
  read(s, caps) {
    return {
      config: ExtensibleConfigCodec.read(s.getStruct(0, 1, 2), caps),
      secrets: ExtensibleConfigCodec.read(s.getStruct(1, 1, 2), caps),
      adapter: DatabaseAdapterConfigCodec.read(s.getStruct(2, 1, 4), caps),
      events: caps.importCap(s.getCapIndex(3)) as T.EventPublisher,
      databases: s.getStructList(4, 0, 2).map((item) => NamedDatabaseCodec.read(item, caps)),
      cancel: caps.importCap(s.getCapIndex(5)) as T.Cancellation,
      storage: caps.importCap(s.getCapIndex(6)) as T.Destination,
    };
  },
};

/**
 * Wire codec for `Entrypoints` (0 data words, 8 pointers).
 *
 * @internal
 */
export const EntrypointsCodec: StructCodec<T.Entrypoints> = {
  dataWords: 0,
  pointerCount: 8,
  write(s, v, caps) {
    s.setCap(0, caps.exportCap(v.eventConsumer));
    s.setCap(1, caps.exportCap(v.jobRunner));
    s.setCap(2, caps.exportCap(v.storefront));
    s.setCap(3, caps.exportCap(v.storage));
    s.setCap(4, caps.exportCap(v.databaseAdapter));
    s.setCap(5, caps.exportCap(v.remoteLibrary));
    s.setCap(6, caps.exportCap(v.cli));
    s.setCap(7, caps.exportCap(v.oidc));
  },
  read(s, caps) {
    return {
      eventConsumer: caps.importCap(s.getCapIndex(0)) as T.EventConsumer,
      jobRunner: caps.importCap(s.getCapIndex(1)) as T.JobRunner,
      storefront: caps.importCap(s.getCapIndex(2)) as T.ContentSource,
      storage: caps.importCap(s.getCapIndex(3)) as T.Destination,
      databaseAdapter: caps.importCap(s.getCapIndex(4)) as T.Database,
      remoteLibrary: caps.importCap(s.getCapIndex(5)) as T.RemoteLibrary,
      cli: caps.importCap(s.getCapIndex(6)) as T.PluginCli,
      oidc: caps.importCap(s.getCapIndex(7)) as T.Oidc,
    };
  },
};

/**
 * Wire codec for `EntrypointsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EntrypointsReplyCodec: StructCodec<T.EntrypointsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          EntrypointsCodec.write(s.initStruct(0, 0, 8), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EntrypointsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: EntrypointsCodec.read(s.getStruct(0, 0, 8), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EntrypointsReply", disc);
    }
  },
};

/**
 * Wire codec for `JobInvocation` (3 data words, 8 pointers).
 *
 * @internal
 */
export const JobInvocationCodec: StructCodec<T.JobInvocation> = {
  dataWords: 3,
  pointerCount: 8,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.payloadSchemaVersion ?? 0);
    s.setText(0, v.invocationId ?? "");
    s.setText(1, v.commandType ?? "");
    s.setText(2, v.payloadJson ?? "");
    s.setText(3, v.idempotencyKey ?? "");
    s.setUint32(1, v.attempt ?? 0);
    s.setText(4, v.correlationId ?? "");
    s.setText(5, v.causationId ?? "");
    s.setUint64(1, BigInt(v.deadlineUnixMs ?? 0));
    s.setText(6, v.checkpointJson ?? "");
    s.setUint32(4, v.checkpointSchemaVersion ?? 0);
    s.setUint32(5, v.invocationSequence ?? 0);
    s.setText(7, v.stepId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      payloadSchemaVersion: s.getUint32(0),
      invocationId: s.getText(0),
      commandType: s.getText(1),
      payloadJson: s.getText(2),
      idempotencyKey: s.getText(3),
      attempt: s.getUint32(1),
      correlationId: s.getText(4),
      causationId: s.getText(5),
      deadlineUnixMs: Number(s.getUint64(1)),
      checkpointJson: s.getText(6),
      checkpointSchemaVersion: s.getUint32(4),
      invocationSequence: s.getUint32(5),
      stepId: s.getText(7),
    };
  },
};

/**
 * Wire codec for `CompletedOutcome` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CompletedOutcomeCodec: StructCodec<T.CompletedOutcome> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
    s.setUint64(0, BigInt(v.bytesCopied ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
      bytesCopied: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `RetryableOutcome` (1 data words, 1 pointers).
 *
 * @internal
 */
export const RetryableOutcomeCodec: StructCodec<T.RetryableOutcome> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
    s.setUint64(0, BigInt(v.retryAfterUnixMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
      retryAfterUnixMs: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `RejectedOutcome` (0 data words, 1 pointers).
 *
 * @internal
 */
export const RejectedOutcomeCodec: StructCodec<T.RejectedOutcome> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
    };
  },
};

/**
 * Wire codec for `CancelledOutcome` (0 data words, 1 pointers).
 *
 * @internal
 */
export const CancelledOutcomeCodec: StructCodec<T.CancelledOutcome> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
    };
  },
};

/**
 * Wire codec for `SuspendedOutcome` (2 data words, 1 pointers).
 *
 * @internal
 */
export const SuspendedOutcomeCodec: StructCodec<T.SuspendedOutcome> = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.checkpointJson ?? "");
    s.setUint32(0, v.checkpointSchemaVersion ?? 0);
    s.setUint64(1, BigInt(v.wakeAtUnixMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      checkpointJson: s.getText(0),
      checkpointSchemaVersion: s.getUint32(0),
      wakeAtUnixMs: Number(s.getUint64(1)),
    };
  },
};

/**
 * Wire codec for `JobOutcome` (1 data words, 1 pointers).
 *
 * @internal
 */
export const JobOutcomeCodec: StructCodec<T.JobOutcome> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "completed":
        s.setUint16(0, 0);
        if (v.value != null) {
          CompletedOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "retryable":
        s.setUint16(0, 1);
        if (v.value != null) {
          RetryableOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "rejected":
        s.setUint16(0, 2);
        if (v.value != null) {
          RejectedOutcomeCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "cancelled":
        s.setUint16(0, 3);
        if (v.value != null) {
          CancelledOutcomeCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "suspended":
        s.setUint16(0, 4);
        if (v.value != null) {
          SuspendedOutcomeCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("JobOutcome", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "completed", value: CompletedOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "retryable", value: RetryableOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 2:
        return { kind: "rejected", value: RejectedOutcomeCodec.read(s.getStruct(0, 0, 1), caps) };
      case 3:
        return { kind: "cancelled", value: CancelledOutcomeCodec.read(s.getStruct(0, 0, 1), caps) };
      case 4:
        return { kind: "suspended", value: SuspendedOutcomeCodec.read(s.getStruct(0, 2, 1), caps) };
      default:
        throw unknownUnion("JobOutcome", disc);
    }
  },
};

/**
 * Wire codec for `DomainEvent` (4 data words, 9 pointers).
 *
 * @internal
 */
export const DomainEventCodec: StructCodec<T.DomainEvent> = {
  dataWords: 4,
  pointerCount: 9,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.eventId ?? "");
    s.setText(1, v.eventType ?? "");
    s.setUint32(0, v.schemaVersion ?? 0);
    s.setUint64(1, BigInt(v.occurredAtUnixMs ?? 0));
    s.setText(2, v.accountId ?? "");
    s.setText(3, v.correlationId ?? "");
    s.setText(4, v.causationId ?? "");
    s.setText(5, v.deduplicationKey ?? "");
    s.setUint32(1, v.deliveryAttempt ?? 0);
    s.setData(6, v.payload ?? EMPTY_BYTES);
    s.setText(7, v.checkpointJson ?? "");
    s.setUint32(4, v.checkpointSchemaVersion ?? 0);
    s.setUint32(5, v.invocationSequence ?? 0);
    s.setBool(192, v.resumePending ?? false);
    s.setText(8, v.source ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      eventId: s.getText(0),
      eventType: s.getText(1),
      schemaVersion: s.getUint32(0),
      occurredAtUnixMs: Number(s.getUint64(1)),
      accountId: s.getText(2),
      correlationId: s.getText(3),
      causationId: s.getText(4),
      deduplicationKey: s.getText(5),
      deliveryAttempt: s.getUint32(1),
      payload: s.getData(6),
      checkpointJson: s.getText(7),
      checkpointSchemaVersion: s.getUint32(4),
      invocationSequence: s.getUint32(5),
      resumePending: s.getBool(192),
      source: s.getText(8),
    };
  },
};

/**
 * Wire codec for `EventAck` (0 data words, 0 pointers).
 *
 * @internal
 */
export const EventAckCodec: StructCodec<T.EventAck> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    return {
      dummy: undefined,
    };
  },
};

/**
 * Wire codec for `EventRetry` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EventRetryCodec: StructCodec<T.EventRetry> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.retryAtUnixMs ?? 0));
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      retryAtUnixMs: Number(s.getUint64(0)),
      reason: s.getText(0),
    };
  },
};

/**
 * Wire codec for `EventReject` (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventRejectCodec: StructCodec<T.EventReject> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      reason: s.getText(0),
    };
  },
};

/**
 * Wire codec for `EventDeadLetter` (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventDeadLetterCodec: StructCodec<T.EventDeadLetter> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      reason: s.getText(0),
    };
  },
};

/**
 * Wire codec for `EventSuspended` (2 data words, 3 pointers).
 *
 * @internal
 */
export const EventSuspendedCodec: StructCodec<T.EventSuspended> = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.checkpointJson ?? "");
    s.setUint32(0, v.checkpointSchemaVersion ?? 0);
    s.setUint64(1, BigInt(v.wakeAtUnixMs ?? 0));
    s.setText(1, v.wakeOnEventType ?? "");
    s.setText(2, v.wakeOnFilterJson ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      checkpointJson: s.getText(0),
      checkpointSchemaVersion: s.getUint32(0),
      wakeAtUnixMs: Number(s.getUint64(1)),
      wakeOnEventType: s.getText(1),
      wakeOnFilterJson: s.getText(2),
    };
  },
};

/**
 * Wire codec for `EventResult` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EventResultCodec: StructCodec<T.EventResult> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ack":
        s.setUint16(0, 0);
        if (v.value != null) {
          EventAckCodec.write(s.initStruct(0, 0, 0), v.value, caps);
        }
        break;
      case "retry":
        s.setUint16(0, 1);
        if (v.value != null) {
          EventRetryCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "reject":
        s.setUint16(0, 2);
        if (v.value != null) {
          EventRejectCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "deadLetter":
        s.setUint16(0, 3);
        if (v.value != null) {
          EventDeadLetterCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "suspended":
        s.setUint16(0, 4);
        if (v.value != null) {
          EventSuspendedCodec.write(s.initStruct(0, 2, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventResult", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ack", value: EventAckCodec.read(s.getStruct(0, 0, 0), caps) };
      case 1:
        return { kind: "retry", value: EventRetryCodec.read(s.getStruct(0, 1, 1), caps) };
      case 2:
        return { kind: "reject", value: EventRejectCodec.read(s.getStruct(0, 0, 1), caps) };
      case 3:
        return { kind: "deadLetter", value: EventDeadLetterCodec.read(s.getStruct(0, 0, 1), caps) };
      case 4:
        return { kind: "suspended", value: EventSuspendedCodec.read(s.getStruct(0, 2, 3), caps) };
      default:
        throw unknownUnion("EventResult", disc);
    }
  },
};

/**
 * Wire codec for `EventBatch` (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventBatchCodec: StructCodec<T.EventBatch> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.events ?? [];
      const items = s.initStructList(0, list.length, 4, 9);
      for (let i = 0; i < items.length; i++) {
        DomainEventCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      events: s.getStructList(0, 4, 9).map((item) => DomainEventCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `EventBatchReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EventBatchReplyCodec: StructCodec<T.EventBatchReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 1, 1);
          for (let i = 0; i < items.length; i++) {
            EventResultCodec.write(items[i]!, list[i]!, caps);
          }
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventBatchReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getStructList(0, 1, 1).map((item) => EventResultCodec.read(item, caps)) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EventBatchReply", disc);
    }
  },
};

/**
 * Wire codec for `JobController` (0 data words, 5 pointers).
 *
 * @internal
 */
export const JobControllerCodec: StructCodec<T.JobController> = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    if (v.invocation != null) {
      JobInvocationCodec.write(s.initStruct(0, 3, 8), v.invocation, caps);
    }
    s.setCap(1, caps.exportCap(v.input));
    s.setCap(2, caps.exportCap(v.output));
    s.setCap(3, caps.exportCap(v.progress));
    s.setCap(4, caps.exportCap(v.cancel));
  },
  read(s, caps) {
    return {
      invocation: JobInvocationCodec.read(s.getStruct(0, 3, 8), caps),
      input: caps.importCap(s.getCapIndex(1)) as T.Source,
      output: caps.importCap(s.getCapIndex(2)) as T.Destination,
      progress: caps.importCap(s.getCapIndex(3)) as T.ProgressSink,
      cancel: caps.importCap(s.getCapIndex(4)) as T.Cancellation,
    };
  },
};

/**
 * Wire codec for `HeadOk` (1 data words, 1 pointers).
 *
 * @internal
 */
export const HeadOkCodec: StructCodec<T.HeadOk> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.meta != null) {
      ObjectMetadataCodec.write(s.initStruct(0, 1, 4), v.meta, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      meta: ObjectMetadataCodec.read(s.getStruct(0, 1, 4), caps),
    };
  },
};

/**
 * Wire codec for `HeadReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const HeadReplyCodec: StructCodec<T.HeadReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          HeadOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HeadReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: HeadOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HeadReply", disc);
    }
  },
};

/**
 * Wire codec for `ListReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ListReplyCodec: StructCodec<T.ListReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ListPageCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ListReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ListPageCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ListReply", disc);
    }
  },
};

/**
 * Wire codec for `GetOk` (0 data words, 2 pointers).
 *
 * @internal
 */
export const GetOkCodec: StructCodec<T.GetOk> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.meta != null) {
      ObjectMetadataCodec.write(s.initStruct(0, 1, 4), v.meta, caps);
    }
    s.setCap(1, caps.exportCap(v.body));
  },
  read(s, caps) {
    return {
      meta: ObjectMetadataCodec.read(s.getStruct(0, 1, 4), caps),
      body: caps.importCap(s.getCapIndex(1)) as T.ByteSource,
    };
  },
};

/**
 * Wire codec for `GetReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const GetReplyCodec: StructCodec<T.GetReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          GetOkCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("GetReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: GetOkCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("GetReply", disc);
    }
  },
};

/**
 * Wire codec for `PutReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PutReplyCodec: StructCodec<T.PutReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PutResultCodec.write(s.initStruct(0, 1, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PutReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PutResultCodec.read(s.getStruct(0, 1, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PutReply", disc);
    }
  },
};

/**
 * Wire codec for `CopyReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CopyReplyCodec: StructCodec<T.CopyReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CopyResultCodec.write(s.initStruct(0, 1, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CopyReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CopyResultCodec.read(s.getStruct(0, 1, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CopyReply", disc);
    }
  },
};

/**
 * Wire codec for `EmptyReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EmptyReplyCodec: StructCodec<T.EmptyReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EmptyReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok" };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EmptyReply", disc);
    }
  },
};

/**
 * Wire codec for `PullOk` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PullOkCodec: StructCodec<T.PullOk> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setData(0, v.chunk ?? EMPTY_BYTES);
    s.setBool(0, v.done ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      chunk: s.getData(0),
      done: s.getBool(0),
    };
  },
};

/**
 * Wire codec for `PullReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PullReplyCodec: StructCodec<T.PullReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PullOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PullReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PullOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PullReply", disc);
    }
  },
};

/**
 * Wire codec for `OpenOk` (0 data words, 2 pointers).
 *
 * @internal
 */
export const OpenOkCodec: StructCodec<T.OpenOk> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.meta != null) {
      ObjectMetadataCodec.write(s.initStruct(0, 1, 4), v.meta, caps);
    }
    s.setCap(1, caps.exportCap(v.body));
  },
  read(s, caps) {
    return {
      meta: ObjectMetadataCodec.read(s.getStruct(0, 1, 4), caps),
      body: caps.importCap(s.getCapIndex(1)) as T.ByteSource,
    };
  },
};

/**
 * Wire codec for `OpenReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const OpenReplyCodec: StructCodec<T.OpenReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          OpenOkCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("OpenReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: OpenOkCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("OpenReply", disc);
    }
  },
};

/**
 * Wire codec for `DescribeReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const DescribeReplyCodec: StructCodec<T.DescribeReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PluginDescribeCodec.write(s.initStruct(0, 2, 10), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DescribeReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PluginDescribeCodec.read(s.getStruct(0, 2, 10), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DescribeReply", disc);
    }
  },
};

/**
 * Wire codec for `HandleReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const HandleReplyCodec: StructCodec<T.HandleReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          JobOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HandleReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: JobOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HandleReply", disc);
    }
  },
};

/**
 * Wire codec for `HealthOk` (1 data words, 1 pointers).
 *
 * @internal
 */
export const HealthOkCodec: StructCodec<T.HealthOk> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.ok ?? false);
    s.setText(0, v.detail ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      ok: s.getBool(0),
      detail: s.getText(0),
    };
  },
};

/**
 * Wire codec for `HealthReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const HealthReplyCodec: StructCodec<T.HealthReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          HealthOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HealthReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: HealthOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HealthReply", disc);
    }
  },
};

/**
 * Wire codec for `AdapterSessionReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterSessionReplyCodec: StructCodec<T.AdapterSessionReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        s.setCap(0, caps.exportCap(v.value));
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("AdapterSessionReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: caps.importCap(s.getCapIndex(0)) as T.AdapterDatabaseSession };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("AdapterSessionReply", disc);
    }
  },
};

/**
 * Wire codec for `GuestDatabaseReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const GuestDatabaseReplyCodec: StructCodec<T.GuestDatabaseReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        s.setCap(0, caps.exportCap(v.value));
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("GuestDatabaseReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: caps.importCap(s.getCapIndex(0)) as T.GuestDatabase };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("GuestDatabaseReply", disc);
    }
  },
};

/**
 * Wire codec for `NamedDatabase` (0 data words, 2 pointers).
 *
 * @internal
 */
export const NamedDatabaseCodec: StructCodec<T.NamedDatabase> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.name ?? "");
    s.setCap(1, caps.exportCap(v.database));
  },
  read(s, caps) {
    return {
      name: s.getText(0),
      database: caps.importCap(s.getCapIndex(1)) as T.GuestDatabase,
    };
  },
};

/**
 * Wire codec for `EventConsumerSpec` (1 data words, 2 pointers).
 *
 * @internal
 */
export const EventConsumerSpecCodec: StructCodec<T.EventConsumerSpec> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.eventType ?? "");
    s.setUint32List(1, v.schemaVersions ?? []);
    s.setBool(0, v.supportsSuspend ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      eventType: s.getText(0),
      schemaVersions: s.getUint32List(1),
      supportsSuspend: s.getBool(0),
    };
  },
};

/**
 * Wire codec for `PluginCapabilities` (0 data words, 6 pointers).
 *
 * @internal
 */
export const PluginCapabilitiesCodec: StructCodec<T.PluginCapabilities> = {
  dataWords: 0,
  pointerCount: 6,
  write(s, v, caps) {
    s.setUint16List(0, (v.entrypoints ?? []).map((v) => ord(A.ENTRYPOINTS, v, "Entrypoint")));
    {
      const list = v.consumes ?? [];
      const items = s.initStructList(1, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        EventConsumerSpecCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setTextList(2, v.produces ?? []);
    s.setTextList(3, v.jobs ?? []);
    s.setTextList(4, v.databases ?? []);
    s.setTextList(5, v.bindings ?? []);
  },
  read(s, caps) {
    return {
      entrypoints: s.getUint16List(0).map((v) => fromOrd(A.ENTRYPOINTS, v, "Entrypoint")),
      consumes: s.getStructList(1, 1, 2).map((item) => EventConsumerSpecCodec.read(item, caps)),
      produces: s.getTextList(2),
      jobs: s.getTextList(3),
      databases: s.getTextList(4),
      bindings: s.getTextList(5),
    };
  },
};

/**
 * Wire codec for `Brand` (0 data words, 6 pointers).
 *
 * @internal
 */
export const BrandCodec: StructCodec<T.Brand> = {
  dataWords: 0,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.id ?? "");
    s.setText(1, v.name ?? "");
    s.setText(2, v.bg ?? "");
    s.setText(3, v.fg ?? "");
    s.setText(4, v.accent ?? "");
    if (v.iconUrl !== undefined) {
      s.setText(5, v.iconUrl ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.Brand = {
      id: s.getText(0),
      name: s.getText(1),
      bg: s.getText(2),
      fg: s.getText(3),
      accent: s.getText(4),
    };
    const iconUrlValue = s.getText(5);
    if (!(iconUrlValue === "")) {
      out.iconUrl = iconUrlValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ConfigOption` (0 data words, 3 pointers).
 *
 * @internal
 */
export const ConfigOptionCodec: StructCodec<T.ConfigOption> = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.key ?? "");
    s.setText(1, v.label ?? "");
    {
      const list = v.values ?? [];
      const items = s.initStructList(2, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        ConfigOptionValueCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      key: s.getText(0),
      label: s.getText(1),
      values: s.getStructList(2, 0, 2).map((item) => ConfigOptionValueCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `ConfigOptionValue` (0 data words, 2 pointers).
 *
 * @internal
 */
export const ConfigOptionValueCodec: StructCodec<T.ConfigOptionValue> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.id ?? "");
    s.setText(1, v.label ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      id: s.getText(0),
      label: s.getText(1),
    };
  },
};

/**
 * Wire codec for `CliSchema` (0 data words, 1 pointers).
 *
 * @internal
 */
export const CliSchemaCodec: StructCodec<T.CliSchema> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.commands ?? [];
      const items = s.initStructList(0, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        CliCommandSpecCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      commands: s.getStructList(0, 0, 3).map((item) => CliCommandSpecCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `CliCommandSpec` (0 data words, 3 pointers).
 *
 * @internal
 */
export const CliCommandSpecCodec: StructCodec<T.CliCommandSpec> = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.name ?? "");
    if (v.about !== undefined) {
      s.setText(1, v.about ?? "");
    }
    {
      const list = v.args ?? [];
      const items = s.initStructList(2, list.length, 1, 5);
      for (let i = 0; i < items.length; i++) {
        CliArgSpecCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    const out: T.CliCommandSpec = {
      name: s.getText(0),
      args: s.getStructList(2, 1, 5).map((item) => CliArgSpecCodec.read(item, caps)),
    };
    const aboutValue = s.getText(1);
    if (!(aboutValue === "")) {
      out.about = aboutValue;
    }
    return out;
  },
};

/**
 * Wire codec for `CliArgSpec` (1 data words, 5 pointers).
 *
 * @internal
 */
export const CliArgSpecCodec: StructCodec<T.CliArgSpec> = {
  dataWords: 1,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    if (v.long !== undefined) {
      s.setText(1, v.long ?? "");
    }
    if (v.short !== undefined) {
      s.setText(2, v.short ?? "");
    }
    s.setUint16(0, ord(A.CLI_ARG_KINDS, v.kind ?? A.CLI_ARG_KINDS[0]!, "CliArgKind"));
    s.setBool(16, v.required ?? false);
    if (v.default !== undefined) {
      s.setText(3, v.default ?? "");
    }
    if (v.about !== undefined) {
      s.setText(4, v.about ?? "");
    }
    s.setBool(17, v.positional ?? false);
  },
  read(s, caps) {
    void caps;
    const out: T.CliArgSpec = {
      name: s.getText(0),
      kind: fromOrd(A.CLI_ARG_KINDS, s.getUint16(0), "CliArgKind"),
      required: s.getBool(16),
      positional: s.getBool(17),
    };
    const longValue = s.getText(1);
    if (!(longValue === "")) {
      out.long = longValue;
    }
    const shortValue = s.getText(2);
    if (!(shortValue === "")) {
      out.short = shortValue;
    }
    const defaultValue = s.getText(3);
    if (!(defaultValue === "")) {
      out.default = defaultValue;
    }
    const aboutValue = s.getText(4);
    if (!(aboutValue === "")) {
      out.about = aboutValue;
    }
    return out;
  },
};

/**
 * Wire codec for `CliArg` (0 data words, 2 pointers).
 *
 * @internal
 */
export const CliArgCodec: StructCodec<T.CliArg> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setText(1, v.value ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      value: s.getText(1),
    };
  },
};

/**
 * Wire codec for `CliInvokeParams` (0 data words, 2 pointers).
 *
 * @internal
 */
export const CliInvokeParamsCodec: StructCodec<T.CliInvokeParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.command ?? "");
    {
      const list = v.args ?? [];
      const items = s.initStructList(1, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        CliArgCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      command: s.getText(0),
      args: s.getStructList(1, 0, 2).map((item) => CliArgCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `CliInvokeResult` (1 data words, 3 pointers).
 *
 * @internal
 */
export const CliInvokeResultCodec: StructCodec<T.CliInvokeResult> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setInt32(0, v.exitCode ?? 0);
    s.setText(0, v.stdout ?? "");
    s.setText(1, v.stderr ?? "");
    if (v.payload != null) {
      ExtensibleConfigCodec.write(s.initStruct(2, 1, 2), v.payload, caps);
    }
  },
  read(s, caps) {
    return {
      exitCode: s.getInt32(0),
      stdout: s.getText(0),
      stderr: s.getText(1),
      payload: ExtensibleConfigCodec.read(s.getStruct(2, 1, 2), caps),
    };
  },
};

/**
 * Wire codec for `CliSchemaReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CliSchemaReplyCodec: StructCodec<T.CliSchemaReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CliSchemaCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CliSchemaReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CliSchemaCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CliSchemaReply", disc);
    }
  },
};

/**
 * Wire codec for `CliInvokeReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CliInvokeReplyCodec: StructCodec<T.CliInvokeReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CliInvokeResultCodec.write(s.initStruct(0, 1, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CliInvokeReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CliInvokeResultCodec.read(s.getStruct(0, 1, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CliInvokeReply", disc);
    }
  },
};

/**
 * Wire codec for `DatabaseAdapterConfig` (1 data words, 4 pointers).
 *
 * @internal
 */
export const DatabaseAdapterConfigCodec: StructCodec<T.DatabaseAdapterConfig> = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    if (v.settings != null) {
      ExtensibleConfigCodec.write(s.initStruct(1, 1, 2), v.settings, caps);
    }
    if (v.binding !== undefined) {
      s.setText(2, v.binding ?? "");
    }
    if (v.instanceId !== undefined) {
      s.setText(3, v.instanceId ?? "");
    }
    s.setBool(0, v.openExisting ?? false);
  },
  read(s, caps) {
    const out: T.DatabaseAdapterConfig = {
      pluginDataDir: s.getText(0),
      settings: ExtensibleConfigCodec.read(s.getStruct(1, 1, 2), caps),
      openExisting: s.getBool(0),
    };
    const bindingValue = s.getText(2);
    if (!(bindingValue === "")) {
      out.binding = bindingValue;
    }
    const instanceIdValue = s.getText(3);
    if (!(instanceIdValue === "")) {
      out.instanceId = instanceIdValue;
    }
    return out;
  },
};

/**
 * Wire codec for `DiagnoseResult` (0 data words, 1 pointers).
 *
 * @internal
 */
export const DiagnoseResultCodec: StructCodec<T.DiagnoseResult> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.lines ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      lines: s.getTextList(0),
    };
  },
};

/**
 * Wire codec for `DiagnoseReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const DiagnoseReplyCodec: StructCodec<T.DiagnoseReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DiagnoseResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DiagnoseReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DiagnoseResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DiagnoseReply", disc);
    }
  },
};

/**
 * Wire codec for `SourceAccount` (1 data words, 4 pointers).
 *
 * @internal
 */
export const SourceAccountCodec: StructCodec<T.SourceAccount> = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setText(1, v.source ?? "");
    s.setText(2, v.marketplace ?? "");
    if (v.label !== undefined) {
      s.setText(3, v.label ?? "");
    }
    s.setBool(0, v.scanEnabled ?? false);
  },
  read(s, caps) {
    void caps;
    const out: T.SourceAccount = {
      accountId: s.getText(0),
      source: s.getText(1),
      marketplace: s.getText(2),
      scanEnabled: s.getBool(0),
    };
    const labelValue = s.getText(3);
    if (!(labelValue === "")) {
      out.label = labelValue;
    }
    return out;
  },
};

/**
 * Wire codec for `LoginParams` (2 data words, 10 pointers).
 *
 * @internal
 */
export const LoginParamsCodec: StructCodec<T.LoginParams> = {
  dataWords: 2,
  pointerCount: 10,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setText(1, v.marketplace ?? "");
    if (v.label !== undefined) {
      s.setText(2, v.label ?? "");
    }
    if (v.email !== undefined) {
      s.setText(3, v.email ?? "");
    }
    if (v.password !== undefined) {
      s.setText(4, v.password ?? "");
    }
    s.setBool(0, v.force ?? false);
    if (v.callbackBind !== undefined) {
      s.setText(5, v.callbackBind ?? "");
    }
    if (v.callbackIpc !== undefined) {
      s.setText(6, v.callbackIpc ?? "");
    }
    if (v.callbackPublicBase !== undefined) {
      s.setText(7, v.callbackPublicBase ?? "");
    }
    s.setBool(1, v.external ?? false);
    if (v.responseUrl !== undefined) {
      s.setText(8, v.responseUrl ?? "");
    }
    s.setBool(2, v.showQr ?? false);
    if (v.timeoutSecs !== undefined) {
      s.setUint64(1, BigInt(v.timeoutSecs ?? 0));
    }
    if (v.extra != null) {
      ExtensibleConfigCodec.write(s.initStruct(9, 1, 2), v.extra, caps);
    }
  },
  read(s, caps) {
    const out: T.LoginParams = {
      pluginDataDir: s.getText(0),
      marketplace: s.getText(1),
      force: s.getBool(0),
      external: s.getBool(1),
      showQr: s.getBool(2),
      extra: ExtensibleConfigCodec.read(s.getStruct(9, 1, 2), caps),
    };
    const labelValue = s.getText(2);
    if (!(labelValue === "")) {
      out.label = labelValue;
    }
    const emailValue = s.getText(3);
    if (!(emailValue === "")) {
      out.email = emailValue;
    }
    const passwordValue = s.getText(4);
    if (!(passwordValue === "")) {
      out.password = passwordValue;
    }
    const callbackBindValue = s.getText(5);
    if (!(callbackBindValue === "")) {
      out.callbackBind = callbackBindValue;
    }
    const callbackIpcValue = s.getText(6);
    if (!(callbackIpcValue === "")) {
      out.callbackIpc = callbackIpcValue;
    }
    const callbackPublicBaseValue = s.getText(7);
    if (!(callbackPublicBaseValue === "")) {
      out.callbackPublicBase = callbackPublicBaseValue;
    }
    const responseUrlValue = s.getText(8);
    if (!(responseUrlValue === "")) {
      out.responseUrl = responseUrlValue;
    }
    const timeoutSecsValue = Number(s.getUint64(1));
    if (!(timeoutSecsValue === 0)) {
      out.timeoutSecs = timeoutSecsValue;
    }
    return out;
  },
};

/**
 * Wire codec for `LoginResult` (0 data words, 2 pointers).
 *
 * @internal
 */
export const LoginResultCodec: StructCodec<T.LoginResult> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.account != null) {
      SourceAccountCodec.write(s.initStruct(0, 1, 4), v.account, caps);
    }
    if (v.credentials !== undefined) {
      s.setData(1, v.credentials ?? EMPTY_BYTES);
    }
  },
  read(s, caps) {
    const out: T.LoginResult = {
      account: SourceAccountCodec.read(s.getStruct(0, 1, 4), caps),
    };
    const credentialsValue = s.getData(1);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  },
};

/**
 * Wire codec for `LoginStartResult` (0 data words, 2 pointers).
 *
 * @internal
 */
export const LoginStartResultCodec: StructCodec<T.LoginStartResult> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.sessionId ?? "");
    s.setText(1, v.url ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      sessionId: s.getText(0),
      url: s.getText(1),
    };
  },
};

/**
 * Wire codec for `LoginCompleteParams` (0 data words, 1 pointers).
 *
 * @internal
 */
export const LoginCompleteParamsCodec: StructCodec<T.LoginCompleteParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.sessionId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      sessionId: s.getText(0),
    };
  },
};

/**
 * Wire codec for `LoginReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const LoginReplyCodec: StructCodec<T.LoginReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          LoginResultCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("LoginReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: LoginResultCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("LoginReply", disc);
    }
  },
};

/**
 * Wire codec for `LoginStartReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const LoginStartReplyCodec: StructCodec<T.LoginStartReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          LoginStartResultCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("LoginStartReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: LoginStartResultCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("LoginStartReply", disc);
    }
  },
};

/**
 * Wire codec for `AccountCredential` (0 data words, 2 pointers).
 *
 * @internal
 */
export const AccountCredentialCodec: StructCodec<T.AccountCredential> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setData(1, v.credentials ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      accountId: s.getText(0),
      credentials: s.getData(1),
    };
  },
};

/**
 * Wire codec for `ScanParams` (1 data words, 3 pointers).
 *
 * @internal
 */
export const ScanParamsCodec: StructCodec<T.ScanParams> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setTextList(1, v.accounts ?? []);
    s.setUint32(0, v.pageSize ?? 0);
    s.setBool(32, v.importEpisodes ?? false);
    s.setBool(33, v.importPlusTitles ?? false);
    {
      const list = v.credentials ?? [];
      const items = s.initStructList(2, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        AccountCredentialCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      pluginDataDir: s.getText(0),
      accounts: s.getTextList(1),
      pageSize: s.getUint32(0),
      importEpisodes: s.getBool(32),
      importPlusTitles: s.getBool(33),
      credentials: s.getStructList(2, 0, 2).map((item) => AccountCredentialCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `ScanBook` (1 data words, 13 pointers).
 *
 * @internal
 */
export const ScanBookCodec: StructCodec<T.ScanBook> = {
  dataWords: 1,
  pointerCount: 13,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setText(1, v.productId ?? "");
    s.setText(2, v.title ?? "");
    if (v.marketplace !== undefined) {
      s.setText(3, v.marketplace ?? "");
    }
    if (v.asin !== undefined) {
      s.setText(4, v.asin ?? "");
    }
    if (v.isbn !== undefined) {
      s.setText(5, v.isbn ?? "");
    }
    if (v.authors !== undefined) {
      s.setText(6, v.authors ?? "");
    }
    if (v.narrators !== undefined) {
      s.setText(7, v.narrators ?? "");
    }
    if (v.series !== undefined) {
      s.setText(8, v.series ?? "");
    }
    if (v.seriesIndex !== undefined) {
      s.setText(9, v.seriesIndex ?? "");
    }
    if (v.contentKind !== undefined) {
      s.setText(10, v.contentKind ?? "");
    }
    if (v.publisher !== undefined) {
      s.setText(11, v.publisher ?? "");
    }
    if (v.lengthMinutes !== undefined) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.subtitle !== undefined) {
      s.setText(12, v.subtitle ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.ScanBook = {
      accountId: s.getText(0),
      productId: s.getText(1),
      title: s.getText(2),
    };
    const marketplaceValue = s.getText(3);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    const asinValue = s.getText(4);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(5);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const authorsValue = s.getText(6);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(7);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(8);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesIndexValue = s.getText(9);
    if (!(seriesIndexValue === "")) {
      out.seriesIndex = seriesIndexValue;
    }
    const contentKindValue = s.getText(10);
    if (!(contentKindValue === "")) {
      out.contentKind = contentKindValue;
    }
    const publisherValue = s.getText(11);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const subtitleValue = s.getText(12);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ScanSummary` (2 data words, 1 pointers).
 *
 * @internal
 */
export const ScanSummaryCodec: StructCodec<T.ScanSummary> = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    s.setUint32(0, v.accounts ?? 0);
    s.setUint32(1, v.booksUpserted ?? 0);
    s.setUint32(2, v.pages ?? 0);
    s.setUint32(3, v.skippedDisabled ?? 0);
    {
      const list = v.books ?? [];
      const items = s.initStructList(0, list.length, 1, 13);
      for (let i = 0; i < items.length; i++) {
        ScanBookCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      accounts: s.getUint32(0),
      booksUpserted: s.getUint32(1),
      pages: s.getUint32(2),
      skippedDisabled: s.getUint32(3),
      books: s.getStructList(0, 1, 13).map((item) => ScanBookCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `ScanReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ScanReplyCodec: StructCodec<T.ScanReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ScanSummaryCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ScanReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ScanSummaryCodec.read(s.getStruct(0, 2, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ScanReply", disc);
    }
  },
};

/**
 * Wire codec for `FetchOptions` (1 data words, 4 pointers).
 *
 * @internal
 */
export const FetchOptionsCodec: StructCodec<T.FetchOptions> = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.widevine ?? false);
    s.setBool(1, v.xheAac ?? false);
    if (v.widevineCdmPath !== undefined) {
      s.setText(0, v.widevineCdmPath ?? "");
    }
    if (v.widevineCdmProvider !== undefined) {
      s.setText(1, v.widevineCdmProvider ?? "");
    }
    s.setBool(2, v.downloadCover ?? false);
    s.setBool(3, v.downloadPdf ?? false);
    s.setText(2, v.coverSize ?? "");
    s.setText(3, v.chapterLayout ?? "");
    s.setBool(4, v.stripAudibleBrandAudio ?? false);
    s.setBool(5, v.downloadClipsBookmarks ?? false);
    s.setBool(6, v.retainAaxFile ?? false);
    s.setUint32(1, v.downloadSpeedLimitKbps ?? 0);
    s.setBool(7, v.saveMetadataJson ?? false);
  },
  read(s, caps) {
    void caps;
    const out: T.FetchOptions = {
      widevine: s.getBool(0),
      xheAac: s.getBool(1),
      downloadCover: s.getBool(2),
      downloadPdf: s.getBool(3),
      coverSize: s.getText(2),
      chapterLayout: s.getText(3),
      stripAudibleBrandAudio: s.getBool(4),
      downloadClipsBookmarks: s.getBool(5),
      retainAaxFile: s.getBool(6),
      downloadSpeedLimitKbps: s.getUint32(1),
      saveMetadataJson: s.getBool(7),
    };
    const widevineCdmPathValue = s.getText(0);
    if (!(widevineCdmPathValue === "")) {
      out.widevineCdmPath = widevineCdmPathValue;
    }
    const widevineCdmProviderValue = s.getText(1);
    if (!(widevineCdmProviderValue === "")) {
      out.widevineCdmProvider = widevineCdmProviderValue;
    }
    return out;
  },
};

/**
 * Wire codec for `FetchTitleParams` (0 data words, 7 pointers).
 *
 * @internal
 */
export const FetchTitleParamsCodec: StructCodec<T.FetchTitleParams> = {
  dataWords: 0,
  pointerCount: 7,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setText(1, v.accountId ?? "");
    s.setText(2, v.titleId ?? "");
    s.setText(3, v.cacheDir ?? "");
    if (v.credentials !== undefined) {
      s.setData(4, v.credentials ?? EMPTY_BYTES);
    }
    if (v.sourceConfig != null) {
      ExtensibleConfigCodec.write(s.initStruct(5, 1, 2), v.sourceConfig, caps);
    }
    if (v.fetch != null) {
      FetchOptionsCodec.write(s.initStruct(6, 1, 4), v.fetch, caps);
    }
  },
  read(s, caps) {
    const out: T.FetchTitleParams = {
      pluginDataDir: s.getText(0),
      accountId: s.getText(1),
      titleId: s.getText(2),
      cacheDir: s.getText(3),
      sourceConfig: ExtensibleConfigCodec.read(s.getStruct(5, 1, 2), caps),
      fetch: FetchOptionsCodec.read(s.getStruct(6, 1, 4), caps),
    };
    const credentialsValue = s.getData(4);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  },
};

/**
 * Wire codec for `PlainPart` (1 data words, 2 pointers).
 *
 * @internal
 */
export const PlainPartCodec: StructCodec<T.PlainPart> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.path ?? "");
    if (v.title !== undefined) {
      s.setText(1, v.title ?? "");
    }
    if (v.durationMs !== undefined) {
      s.setUint64(0, BigInt(v.durationMs ?? 0));
    }
  },
  read(s, caps) {
    void caps;
    const out: T.PlainPart = {
      path: s.getText(0),
    };
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const durationMsValue = Number(s.getUint64(0));
    if (!(durationMsValue === 0)) {
      out.durationMs = durationMsValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ChapterMarker` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ChapterMarkerCodec: StructCodec<T.ChapterMarker> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.title ?? "");
    s.setUint64(0, BigInt(v.startMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      title: s.getText(0),
      startMs: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `PlainFetch` (0 data words, 5 pointers).
 *
 * @internal
 */
export const PlainFetchCodec: StructCodec<T.PlainFetch> = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    {
      const list = v.parts ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        PlainPartCodec.write(items[i]!, list[i]!, caps);
      }
    }
    if (v.m4bPath !== undefined) {
      s.setText(1, v.m4bPath ?? "");
    }
    if (v.coverPath !== undefined) {
      s.setText(2, v.coverPath ?? "");
    }
    {
      const list = v.chapters ?? [];
      const items = s.initStructList(3, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        ChapterMarkerCodec.write(items[i]!, list[i]!, caps);
      }
    }
    if (v.pdfUrl !== undefined) {
      s.setText(4, v.pdfUrl ?? "");
    }
  },
  read(s, caps) {
    const out: T.PlainFetch = {
      parts: s.getStructList(0, 1, 2).map((item) => PlainPartCodec.read(item, caps)),
      chapters: s.getStructList(3, 1, 1).map((item) => ChapterMarkerCodec.read(item, caps)),
    };
    const m4bPathValue = s.getText(1);
    if (!(m4bPathValue === "")) {
      out.m4bPath = m4bPathValue;
    }
    const coverPathValue = s.getText(2);
    if (!(coverPathValue === "")) {
      out.coverPath = coverPathValue;
    }
    const pdfUrlValue = s.getText(4);
    if (!(pdfUrlValue === "")) {
      out.pdfUrl = pdfUrlValue;
    }
    return out;
  },
};

/**
 * Wire codec for `FetchTitleReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const FetchTitleReplyCodec: StructCodec<T.FetchTitleReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PlainFetchCodec.write(s.initStruct(0, 0, 5), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("FetchTitleReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PlainFetchCodec.read(s.getStruct(0, 0, 5), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("FetchTitleReply", disc);
    }
  },
};

/**
 * Wire codec for `SourceAccounts` (0 data words, 1 pointers).
 *
 * @internal
 */
export const SourceAccountsCodec: StructCodec<T.SourceAccounts> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.accounts ?? [];
      const items = s.initStructList(0, list.length, 1, 4);
      for (let i = 0; i < items.length; i++) {
        SourceAccountCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      accounts: s.getStructList(0, 1, 4).map((item) => SourceAccountCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `SourceAccountsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const SourceAccountsReplyCodec: StructCodec<T.SourceAccountsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          SourceAccountsCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("SourceAccountsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: SourceAccountsCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("SourceAccountsReply", disc);
    }
  },
};

/**
 * Wire codec for `SearchCatalogParams` (2 data words, 3 pointers).
 *
 * @internal
 */
export const SearchCatalogParamsCodec: StructCodec<T.SearchCatalogParams> = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.query ?? "");
    s.setText(1, v.region ?? "");
    s.setUint32(0, v.limit ?? 0);
    s.setUint32(1, v.page ?? 0);
    s.setUint16(4, ord(A.CATALOG_SORTS, v.sort ?? A.CATALOG_SORTS[0]!, "CatalogSort"));
    s.setUint16(5, ord(A.CATALOG_FIELDS, v.field ?? A.CATALOG_FIELDS[0]!, "CatalogField"));
    if (v.language !== undefined) {
      s.setText(2, v.language ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.SearchCatalogParams = {
      query: s.getText(0),
      region: s.getText(1),
      limit: s.getUint32(0),
      page: s.getUint32(1),
      sort: fromOrd(A.CATALOG_SORTS, s.getUint16(4), "CatalogSort"),
      field: fromOrd(A.CATALOG_FIELDS, s.getUint16(5), "CatalogField"),
    };
    const languageValue = s.getText(2);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ExpandCandidatesParams` (1 data words, 10 pointers).
 *
 * @internal
 */
export const ExpandCandidatesParamsCodec: StructCodec<T.ExpandCandidatesParams> = {
  dataWords: 1,
  pointerCount: 10,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.source ?? "");
    s.setText(1, v.productId ?? "");
    s.setText(2, v.title ?? "");
    if (v.authors !== undefined) {
      s.setText(3, v.authors ?? "");
    }
    if (v.narrators !== undefined) {
      s.setText(4, v.narrators ?? "");
    }
    if (v.series !== undefined) {
      s.setText(5, v.series ?? "");
    }
    if (v.seriesAsin !== undefined) {
      s.setText(6, v.seriesAsin ?? "");
    }
    if (v.asin !== undefined) {
      s.setText(7, v.asin ?? "");
    }
    if (v.isbn !== undefined) {
      s.setText(8, v.isbn ?? "");
    }
    s.setText(9, v.region ?? "");
    s.setUint32(0, v.limit ?? 0);
  },
  read(s, caps) {
    void caps;
    const out: T.ExpandCandidatesParams = {
      source: s.getText(0),
      productId: s.getText(1),
      title: s.getText(2),
      region: s.getText(9),
      limit: s.getUint32(0),
    };
    const authorsValue = s.getText(3);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(4);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(5);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesAsinValue = s.getText(6);
    if (!(seriesAsinValue === "")) {
      out.seriesAsin = seriesAsinValue;
    }
    const asinValue = s.getText(7);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(8);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  },
};

/**
 * Wire codec for `PurchaseHintParams` (1 data words, 6 pointers).
 *
 * @internal
 */
export const PurchaseHintParamsCodec: StructCodec<T.PurchaseHintParams> = {
  dataWords: 1,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    if (v.productId !== undefined) {
      s.setText(0, v.productId ?? "");
    }
    if (v.title !== undefined) {
      s.setText(1, v.title ?? "");
    }
    if (v.authors !== undefined) {
      s.setText(2, v.authors ?? "");
    }
    if (v.asin !== undefined) {
      s.setText(3, v.asin ?? "");
    }
    if (v.isbn !== undefined) {
      s.setText(4, v.isbn ?? "");
    }
    s.setText(5, v.region ?? "");
    s.setBool(0, v.withPrice ?? false);
  },
  read(s, caps) {
    void caps;
    const out: T.PurchaseHintParams = {
      region: s.getText(5),
      withPrice: s.getBool(0),
    };
    const productIdValue = s.getText(0);
    if (!(productIdValue === "")) {
      out.productId = productIdValue;
    }
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const authorsValue = s.getText(2);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const asinValue = s.getText(3);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(4);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ListDealsParams` (1 data words, 0 pointers).
 *
 * @internal
 */
export const ListDealsParamsCodec: StructCodec<T.ListDealsParams> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    if (v.limit !== undefined) {
      s.setUint32(0, v.limit ?? 0);
    }
  },
  read(s, caps) {
    void caps;
    const out: T.ListDealsParams = {
    };
    const limitValue = s.getUint32(0);
    if (!(limitValue === 0)) {
      out.limit = limitValue;
    }
    return out;
  },
};

/**
 * Wire codec for `CatalogDetailParams` (0 data words, 2 pointers).
 *
 * @internal
 */
export const CatalogDetailParamsCodec: StructCodec<T.CatalogDetailParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    if (v.isbn !== undefined) {
      s.setText(1, v.isbn ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.CatalogDetailParams = {
      productId: s.getText(0),
    };
    const isbnValue = s.getText(1);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  },
};

/**
 * Wire codec for `CatalogHit` (5 data words, 19 pointers).
 *
 * @internal
 */
export const CatalogHitCodec: StructCodec<T.CatalogHit> = {
  dataWords: 5,
  pointerCount: 19,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    s.setText(1, v.title ?? "");
    if (v.authors !== undefined) {
      s.setText(2, v.authors ?? "");
    }
    if (v.narrators !== undefined) {
      s.setText(3, v.narrators ?? "");
    }
    if (v.series !== undefined) {
      s.setText(4, v.series ?? "");
    }
    if (v.seriesIndex !== undefined) {
      s.setText(5, v.seriesIndex ?? "");
    }
    if (v.asin !== undefined) {
      s.setText(6, v.asin ?? "");
    }
    if (v.isbn !== undefined) {
      s.setText(7, v.isbn ?? "");
    }
    if (v.url !== undefined) {
      s.setText(8, v.url ?? "");
    }
    if (v.coverUrl !== undefined) {
      s.setText(9, v.coverUrl ?? "");
    }
    s.setText(10, v.origin ?? "");
    if (v.subtitle !== undefined) {
      s.setText(11, v.subtitle ?? "");
    }
    if (v.description !== undefined) {
      s.setText(12, v.description ?? "");
    }
    if (v.publisher !== undefined) {
      s.setText(13, v.publisher ?? "");
    }
    if (v.lengthMinutes !== undefined) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.publishedAt !== undefined) {
      s.setText(14, v.publishedAt ?? "");
    }
    if (v.categories !== undefined) {
      s.setText(15, v.categories ?? "");
    }
    if (v.language !== undefined) {
      s.setText(16, v.language ?? "");
    }
    if (v.priceCents !== undefined) {
      s.setInt64(1, v.priceCents ?? 0);
    }
    if (v.currency !== undefined) {
      s.setText(17, v.currency ?? "");
    }
    if (v.priceLabel !== undefined) {
      s.setText(18, v.priceLabel ?? "");
    }
    if (v.ratingOverall !== undefined) {
      s.setFloat64(2, v.ratingOverall ?? 0);
    }
    if (v.ratingCount !== undefined) {
      s.setInt64(3, v.ratingCount ?? 0);
    }
    s.setUint16(16, ord(A.ABRIDGEMENTS, v.abridgement ?? A.ABRIDGEMENTS[0]!, "Abridgement"));
  },
  read(s, caps) {
    void caps;
    const out: T.CatalogHit = {
      productId: s.getText(0),
      title: s.getText(1),
      origin: s.getText(10),
      abridgement: fromOrd(A.ABRIDGEMENTS, s.getUint16(16), "Abridgement"),
    };
    const authorsValue = s.getText(2);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(3);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(4);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesIndexValue = s.getText(5);
    if (!(seriesIndexValue === "")) {
      out.seriesIndex = seriesIndexValue;
    }
    const asinValue = s.getText(6);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(7);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const urlValue = s.getText(8);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const coverUrlValue = s.getText(9);
    if (!(coverUrlValue === "")) {
      out.coverUrl = coverUrlValue;
    }
    const subtitleValue = s.getText(11);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const descriptionValue = s.getText(12);
    if (!(descriptionValue === "")) {
      out.description = descriptionValue;
    }
    const publisherValue = s.getText(13);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const publishedAtValue = s.getText(14);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const categoriesValue = s.getText(15);
    if (!(categoriesValue === "")) {
      out.categories = categoriesValue;
    }
    const languageValue = s.getText(16);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const priceCentsValue = s.getInt64(1);
    if (!(priceCentsValue === 0n)) {
      out.priceCents = priceCentsValue;
    }
    const currencyValue = s.getText(17);
    if (!(currencyValue === "")) {
      out.currency = currencyValue;
    }
    const priceLabelValue = s.getText(18);
    if (!(priceLabelValue === "")) {
      out.priceLabel = priceLabelValue;
    }
    const ratingOverallValue = s.getFloat64(2);
    if (!(ratingOverallValue === 0)) {
      out.ratingOverall = ratingOverallValue;
    }
    const ratingCountValue = s.getInt64(3);
    if (!(ratingCountValue === 0n)) {
      out.ratingCount = ratingCountValue;
    }
    return out;
  },
};

/**
 * Wire codec for `CatalogHits` (0 data words, 1 pointers).
 *
 * @internal
 */
export const CatalogHitsCodec: StructCodec<T.CatalogHits> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.hits ?? [];
      const items = s.initStructList(0, list.length, 5, 19);
      for (let i = 0; i < items.length; i++) {
        CatalogHitCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      hits: s.getStructList(0, 5, 19).map((item) => CatalogHitCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `CatalogHitsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CatalogHitsReplyCodec: StructCodec<T.CatalogHitsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CatalogHitsCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CatalogHitsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CatalogHitsCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CatalogHitsReply", disc);
    }
  },
};

/**
 * Wire codec for `CatalogDetail` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CatalogDetailCodec: StructCodec<T.CatalogDetail> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.hit != null) {
      CatalogHitCodec.write(s.initStruct(0, 5, 19), v.hit, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hit: CatalogHitCodec.read(s.getStruct(0, 5, 19), caps),
    };
  },
};

/**
 * Wire codec for `CatalogDetailReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const CatalogDetailReplyCodec: StructCodec<T.CatalogDetailReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CatalogDetailCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CatalogDetailReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CatalogDetailCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CatalogDetailReply", disc);
    }
  },
};

/**
 * Wire codec for `PurchaseHint` (3 data words, 7 pointers).
 *
 * @internal
 */
export const PurchaseHintCodec: StructCodec<T.PurchaseHint> = {
  dataWords: 3,
  pointerCount: 7,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    if (v.title !== undefined) {
      s.setText(1, v.title ?? "");
    }
    if (v.url !== undefined) {
      s.setText(2, v.url ?? "");
    }
    if (v.priceCents !== undefined) {
      s.setInt64(0, v.priceCents ?? 0);
    }
    if (v.currency !== undefined) {
      s.setText(3, v.currency ?? "");
    }
    if (v.priceLabel !== undefined) {
      s.setText(4, v.priceLabel ?? "");
    }
    if (v.listPriceCents !== undefined) {
      s.setInt64(1, v.listPriceCents ?? 0);
    }
    if (v.listPriceLabel !== undefined) {
      s.setText(5, v.listPriceLabel ?? "");
    }
    if (v.memberPriceCents !== undefined) {
      s.setInt64(2, v.memberPriceCents ?? 0);
    }
    if (v.memberPriceLabel !== undefined) {
      s.setText(6, v.memberPriceLabel ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.PurchaseHint = {
      productId: s.getText(0),
    };
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const urlValue = s.getText(2);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const priceCentsValue = s.getInt64(0);
    if (!(priceCentsValue === 0n)) {
      out.priceCents = priceCentsValue;
    }
    const currencyValue = s.getText(3);
    if (!(currencyValue === "")) {
      out.currency = currencyValue;
    }
    const priceLabelValue = s.getText(4);
    if (!(priceLabelValue === "")) {
      out.priceLabel = priceLabelValue;
    }
    const listPriceCentsValue = s.getInt64(1);
    if (!(listPriceCentsValue === 0n)) {
      out.listPriceCents = listPriceCentsValue;
    }
    const listPriceLabelValue = s.getText(5);
    if (!(listPriceLabelValue === "")) {
      out.listPriceLabel = listPriceLabelValue;
    }
    const memberPriceCentsValue = s.getInt64(2);
    if (!(memberPriceCentsValue === 0n)) {
      out.memberPriceCents = memberPriceCentsValue;
    }
    const memberPriceLabelValue = s.getText(6);
    if (!(memberPriceLabelValue === "")) {
      out.memberPriceLabel = memberPriceLabelValue;
    }
    return out;
  },
};

/**
 * Wire codec for `PurchaseHintResult` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PurchaseHintResultCodec: StructCodec<T.PurchaseHintResult> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.hint != null) {
      PurchaseHintCodec.write(s.initStruct(0, 3, 7), v.hint, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hint: PurchaseHintCodec.read(s.getStruct(0, 3, 7), caps),
    };
  },
};

/**
 * Wire codec for `PurchaseHintReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PurchaseHintReplyCodec: StructCodec<T.PurchaseHintReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PurchaseHintResultCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PurchaseHintReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PurchaseHintResultCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PurchaseHintReply", disc);
    }
  },
};

/**
 * Wire codec for `ScanLibraryParams` (1 data words, 0 pointers).
 *
 * @internal
 */
export const ScanLibraryParamsCodec: StructCodec<T.ScanLibraryParams> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.force ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      force: s.getBool(0),
    };
  },
};

/**
 * Wire codec for `AuthenticateUserParams` (0 data words, 2 pointers).
 *
 * @internal
 */
export const AuthenticateUserParamsCodec: StructCodec<T.AuthenticateUserParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.username ?? "");
    s.setText(1, v.password ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      username: s.getText(0),
      password: s.getText(1),
    };
  },
};

/**
 * Wire codec for `ExternalUser` (0 data words, 4 pointers).
 *
 * @internal
 */
export const ExternalUserCodec: StructCodec<T.ExternalUser> = {
  dataWords: 0,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.provider ?? "");
    s.setText(1, v.externalUserId ?? "");
    if (v.displayName !== undefined) {
      s.setText(2, v.displayName ?? "");
    }
    if (v.accessToken !== undefined) {
      s.setText(3, v.accessToken ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out: T.ExternalUser = {
      provider: s.getText(0),
      externalUserId: s.getText(1),
    };
    const displayNameValue = s.getText(2);
    if (!(displayNameValue === "")) {
      out.displayName = displayNameValue;
    }
    const accessTokenValue = s.getText(3);
    if (!(accessTokenValue === "")) {
      out.accessToken = accessTokenValue;
    }
    return out;
  },
};

/**
 * Wire codec for `ExternalUserReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ExternalUserReplyCodec: StructCodec<T.ExternalUserReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ExternalUserCodec.write(s.initStruct(0, 0, 4), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExternalUserReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ExternalUserCodec.read(s.getStruct(0, 0, 4), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ExternalUserReply", disc);
    }
  },
};

/**
 * Wire codec for `EventPollResult` (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventPollResultCodec: StructCodec<T.EventPollResult> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.users ?? [];
      const items = s.initStructList(0, list.length, 0, 4);
      for (let i = 0; i < items.length; i++) {
        ExternalUserCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      users: s.getStructList(0, 0, 4).map((item) => ExternalUserCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `EventPollReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const EventPollReplyCodec: StructCodec<T.EventPollReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          EventPollResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventPollReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: EventPollResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EventPollReply", disc);
    }
  },
};

/**
 * Wire codec for `ListeningProgress` (6 data words, 6 pointers).
 *
 * @internal
 */
export const ListeningProgressCodec: StructCodec<T.ListeningProgress> = {
  dataWords: 6,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.externalUserId ?? "");
    s.setText(1, v.externalItemId ?? "");
    if (v.identityId !== undefined) {
      s.setInt64(0, v.identityId ?? 0);
    }
    if (v.title !== undefined) {
      s.setText(2, v.title ?? "");
    }
    if (v.authors !== undefined) {
      s.setText(3, v.authors ?? "");
    }
    if (v.asin !== undefined) {
      s.setText(4, v.asin ?? "");
    }
    if (v.isbn !== undefined) {
      s.setText(5, v.isbn ?? "");
    }
    if (v.progress !== undefined) {
      s.setFloat64(1, v.progress ?? 0);
    }
    if (v.currentTimeSeconds !== undefined) {
      s.setFloat64(2, v.currentTimeSeconds ?? 0);
    }
    if (v.durationSeconds !== undefined) {
      s.setFloat64(3, v.durationSeconds ?? 0);
    }
    s.setBool(256, v.isFinished ?? false);
    if (v.lastListenedAtUnixMs !== undefined) {
      s.setUint64(5, BigInt(v.lastListenedAtUnixMs ?? 0));
    }
  },
  read(s, caps) {
    void caps;
    const out: T.ListeningProgress = {
      externalUserId: s.getText(0),
      externalItemId: s.getText(1),
      isFinished: s.getBool(256),
    };
    const identityIdValue = s.getInt64(0);
    if (!(identityIdValue === 0n)) {
      out.identityId = identityIdValue;
    }
    const titleValue = s.getText(2);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const authorsValue = s.getText(3);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const asinValue = s.getText(4);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(5);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const progressValue = s.getFloat64(1);
    if (!(progressValue === 0)) {
      out.progress = progressValue;
    }
    const currentTimeSecondsValue = s.getFloat64(2);
    if (!(currentTimeSecondsValue === 0)) {
      out.currentTimeSeconds = currentTimeSecondsValue;
    }
    const durationSecondsValue = s.getFloat64(3);
    if (!(durationSecondsValue === 0)) {
      out.durationSeconds = durationSecondsValue;
    }
    const lastListenedAtUnixMsValue = Number(s.getUint64(5));
    if (!(lastListenedAtUnixMsValue === 0)) {
      out.lastListenedAtUnixMs = lastListenedAtUnixMsValue;
    }
    return out;
  },
};

/**
 * Wire codec for `SyncListeningResult` (0 data words, 1 pointers).
 *
 * @internal
 */
export const SyncListeningResultCodec: StructCodec<T.SyncListeningResult> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.items ?? [];
      const items = s.initStructList(0, list.length, 6, 6);
      for (let i = 0; i < items.length; i++) {
        ListeningProgressCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      items: s.getStructList(0, 6, 6).map((item) => ListeningProgressCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `SyncListeningReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const SyncListeningReplyCodec: StructCodec<T.SyncListeningReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          SyncListeningResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("SyncListeningReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: SyncListeningResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("SyncListeningReply", disc);
    }
  },
};

/**
 * Wire codec for `PluginEvent` (2 data words, 5 pointers).
 *
 * @internal
 */
export const PluginEventCodec: StructCodec<T.PluginEvent> = {
  dataWords: 2,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.eventType ?? "");
    s.setUint32(0, v.schemaVersion ?? 0);
    s.setText(1, v.deduplicationKey ?? "");
    s.setData(2, v.payload ?? EMPTY_BYTES);
    s.setUint64(1, BigInt(v.occurredAtUnixMs ?? 0));
    s.setText(3, v.correlationId ?? "");
    s.setText(4, v.causationId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      eventType: s.getText(0),
      schemaVersion: s.getUint32(0),
      deduplicationKey: s.getText(1),
      payload: s.getData(2),
      occurredAtUnixMs: Number(s.getUint64(1)),
      correlationId: s.getText(3),
      causationId: s.getText(4),
    };
  },
};

/**
 * Wire codec for `PublishOk` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PublishOkCodec: StructCodec<T.PublishOk> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.eventId ?? "");
    s.setBool(0, v.duplicate ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      eventId: s.getText(0),
      duplicate: s.getBool(0),
    };
  },
};

/**
 * Wire codec for `PublishReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PublishReplyCodec: StructCodec<T.PublishReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PublishOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PublishReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PublishOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PublishReply", disc);
    }
  },
};

/**
 * Wire codec for `Invocation` (1 data words, 4 pointers).
 *
 * @internal
 */
export const InvocationCodec: StructCodec<T.Invocation> = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.id ?? "");
    s.setText(1, v.accountId ?? "");
    s.setUint64(0, BigInt(v.deadlineUnixMs ?? 0));
    s.setText(2, v.correlationId ?? "");
    s.setText(3, v.causationId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      id: s.getText(0),
      accountId: s.getText(1),
      deadlineUnixMs: Number(s.getUint64(0)),
      correlationId: s.getText(2),
      causationId: s.getText(3),
    };
  },
};

/**
 * Wire codec for `DbValue` (2 data words, 1 pointers).
 *
 * @internal
 */
export const DbValueCodec: StructCodec<T.DbValue> = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    switch (v.kind) {
      case "null":
        s.setUint16(1, 0);
        s.setUint16(0, ord(A.DB_TYPES, v.value ?? A.DB_TYPES[0]!, "DbType"));
        break;
      case "boolean":
        s.setUint16(1, 1);
        s.setBool(0, v.value ?? false);
        break;
      case "int64":
        s.setUint16(1, 2);
        s.setInt64(1, v.value ?? 0);
        break;
      case "float64":
        s.setUint16(1, 3);
        s.setFloat64(1, v.value ?? 0);
        break;
      case "text":
        s.setUint16(1, 4);
        s.setText(0, v.value ?? "");
        break;
      case "bytes":
        s.setUint16(1, 5);
        s.setData(0, v.value ?? EMPTY_BYTES);
        break;
      default:
        throw unknownUnion("DbValue", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    void caps;
    const disc = s.getUint16(1);
    switch (disc) {
      case 0:
        return { kind: "null", value: fromOrd(A.DB_TYPES, s.getUint16(0), "DbType") };
      case 1:
        return { kind: "boolean", value: s.getBool(0) };
      case 2:
        return { kind: "int64", value: s.getInt64(1) };
      case 3:
        return { kind: "float64", value: s.getFloat64(1) };
      case 4:
        return { kind: "text", value: s.getText(0) };
      case 5:
        return { kind: "bytes", value: s.getData(0) };
      default:
        throw unknownUnion("DbValue", disc);
    }
  },
};

/**
 * Wire codec for `DbColumn` (1 data words, 1 pointers).
 *
 * @internal
 */
export const DbColumnCodec: StructCodec<T.DbColumn> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint16(0, ord(A.DB_TYPES, v.dbType ?? A.DB_TYPES[0]!, "DbType"));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      dbType: fromOrd(A.DB_TYPES, s.getUint16(0), "DbType"),
    };
  },
};

/**
 * Wire codec for `DbRow` (0 data words, 1 pointers).
 *
 * @internal
 */
export const DbRowCodec: StructCodec<T.DbRow> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.values ?? [];
      const items = s.initStructList(0, list.length, 2, 1);
      for (let i = 0; i < items.length; i++) {
        DbValueCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      values: s.getStructList(0, 2, 1).map((item) => DbValueCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `DbStatement` (1 data words, 2 pointers).
 *
 * @internal
 */
export const DbStatementCodec: StructCodec<T.DbStatement> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.sql ?? "");
    {
      const list = v.parameters ?? [];
      const items = s.initStructList(1, list.length, 2, 1);
      for (let i = 0; i < items.length; i++) {
        DbValueCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setUint16(0, ord(A.DB_STATEMENT_KINDS, v.kind ?? A.DB_STATEMENT_KINDS[0]!, "DbStatementKind"));
    s.setUint32(1, v.maxRows ?? 0);
    s.setUint16(1, ord(A.DB_RESULT_SELECTIONS, v.resultSelection ?? A.DB_RESULT_SELECTIONS[0]!, "DbResultSelection"));
  },
  read(s, caps) {
    return {
      sql: s.getText(0),
      parameters: s.getStructList(1, 2, 1).map((item) => DbValueCodec.read(item, caps)),
      kind: fromOrd(A.DB_STATEMENT_KINDS, s.getUint16(0), "DbStatementKind"),
      maxRows: s.getUint32(1),
      resultSelection: fromOrd(A.DB_RESULT_SELECTIONS, s.getUint16(1), "DbResultSelection"),
    };
  },
};

/**
 * Wire codec for `ExecuteRequest` (1 data words, 3 pointers).
 *
 * @internal
 */
export const ExecuteRequestCodec: StructCodec<T.ExecuteRequest> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.operationId ?? "");
    s.setText(1, v.requestHash ?? "");
    {
      const list = v.statements ?? [];
      const items = s.initStructList(2, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        DbStatementCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setUint64(0, BigInt(v.deadlineUnixMs ?? 0));
  },
  read(s, caps) {
    return {
      operationId: s.getText(0),
      requestHash: s.getText(1),
      statements: s.getStructList(2, 1, 2).map((item) => DbStatementCodec.read(item, caps)),
      deadlineUnixMs: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `SqlSpan` (1 data words, 0 pointers).
 *
 * @internal
 */
export const SqlSpanCodec: StructCodec<T.SqlSpan> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.start ?? 0);
    s.setUint32(1, v.end ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      start: s.getUint32(0),
      end: s.getUint32(1),
    };
  },
};

/**
 * Wire codec for `TextCollateSite` (0 data words, 1 pointers).
 *
 * @internal
 */
export const TextCollateSiteCodec: StructCodec<T.TextCollateSite> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.span != null) {
      SqlSpanCodec.write(s.initStruct(0, 1, 0), v.span, caps);
    }
  },
  read(s, caps) {
    return {
      span: SqlSpanCodec.read(s.getStruct(0, 1, 0), caps),
    };
  },
};

/**
 * Wire codec for `IntegerArithSite` (1 data words, 3 pointers).
 *
 * @internal
 */
export const IntegerArithSiteCodec: StructCodec<T.IntegerArithSite> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    if (v.full != null) {
      SqlSpanCodec.write(s.initStruct(0, 1, 0), v.full, caps);
    }
    if (v.lhs != null) {
      SqlSpanCodec.write(s.initStruct(1, 1, 0), v.lhs, caps);
    }
    if (v.rhs != null) {
      SqlSpanCodec.write(s.initStruct(2, 1, 0), v.rhs, caps);
    }
    s.setUint16(0, ord(A.INTEGER_ARITH_KINDS, v.kind ?? A.INTEGER_ARITH_KINDS[0]!, "IntegerArithKind"));
  },
  read(s, caps) {
    return {
      full: SqlSpanCodec.read(s.getStruct(0, 1, 0), caps),
      lhs: SqlSpanCodec.read(s.getStruct(1, 1, 0), caps),
      rhs: SqlSpanCodec.read(s.getStruct(2, 1, 0), caps),
      kind: fromOrd(A.INTEGER_ARITH_KINDS, s.getUint16(0), "IntegerArithKind"),
    };
  },
};

/**
 * Wire codec for `PhysicalAccess` (0 data words, 2 pointers).
 *
 * @internal
 */
export const PhysicalAccessCodec: StructCodec<T.PhysicalAccess> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setText(1, v.column ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      column: s.getText(1),
    };
  },
};

/**
 * Wire codec for `ResolvedAssignment` (1 data words, 2 pointers).
 *
 * @internal
 */
export const ResolvedAssignmentCodec: StructCodec<T.ResolvedAssignment> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setText(1, v.column ?? "");
    s.setUint16(0, ord(A.RESOLVED_SQL_TYPES, v.dest ?? A.RESOLVED_SQL_TYPES[0]!, "ResolvedSqlType"));
    s.setUint16(1, ord(A.RESOLVED_SQL_TYPES, v.source ?? A.RESOLVED_SQL_TYPES[0]!, "ResolvedSqlType"));
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      column: s.getText(1),
      dest: fromOrd(A.RESOLVED_SQL_TYPES, s.getUint16(0), "ResolvedSqlType"),
      source: fromOrd(A.RESOLVED_SQL_TYPES, s.getUint16(1), "ResolvedSqlType"),
    };
  },
};

/**
 * Wire codec for `NamedSqlType` (1 data words, 1 pointers).
 *
 * @internal
 */
export const NamedSqlTypeCodec: StructCodec<T.NamedSqlType> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint16(0, ord(A.RESOLVED_SQL_TYPES, v.sqlType ?? A.RESOLVED_SQL_TYPES[0]!, "ResolvedSqlType"));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      sqlType: fromOrd(A.RESOLVED_SQL_TYPES, s.getUint16(0), "ResolvedSqlType"),
    };
  },
};

/**
 * Wire codec for `ColumnReference` (0 data words, 2 pointers).
 *
 * @internal
 */
export const ColumnReferenceCodec: StructCodec<T.ColumnReference> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.refTable ?? "");
    s.setTextList(1, v.refColumns ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      refTable: s.getText(0),
      refColumns: s.getTextList(1),
    };
  },
};

/**
 * Wire codec for `OptionalColumnReference` (1 data words, 1 pointers).
 *
 * @internal
 */
export const OptionalColumnReferenceCodec: StructCodec<T.OptionalColumnReference> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "none":
        s.setUint16(0, 0);
        break;
      case "some":
        s.setUint16(0, 1);
        if (v.value != null) {
          ColumnReferenceCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("OptionalColumnReference", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "none" };
      case 1:
        return { kind: "some", value: ColumnReferenceCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("OptionalColumnReference", disc);
    }
  },
};

/**
 * Wire codec for `ForeignKeyConstraint` (0 data words, 3 pointers).
 *
 * @internal
 */
export const ForeignKeyConstraintCodec: StructCodec<T.ForeignKeyConstraint> = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.columns ?? []);
    s.setText(1, v.refTable ?? "");
    s.setTextList(2, v.refColumns ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      columns: s.getTextList(0),
      refTable: s.getText(1),
      refColumns: s.getTextList(2),
    };
  },
};

/**
 * Wire codec for `TableConstraint` (1 data words, 1 pointers).
 *
 * @internal
 */
export const TableConstraintCodec: StructCodec<T.TableConstraint> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "primaryKey":
        s.setUint16(0, 0);
        s.setTextList(0, v.value ?? []);
        break;
      case "unique":
        s.setUint16(0, 1);
        s.setTextList(0, v.value ?? []);
        break;
      case "check":
        s.setUint16(0, 2);
        s.setText(0, v.value ?? "");
        break;
      case "foreignKey":
        s.setUint16(0, 3);
        if (v.value != null) {
          ForeignKeyConstraintCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("TableConstraint", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "primaryKey", value: s.getTextList(0) };
      case 1:
        return { kind: "unique", value: s.getTextList(0) };
      case 2:
        return { kind: "check", value: s.getText(0) };
      case 3:
        return { kind: "foreignKey", value: ForeignKeyConstraintCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("TableConstraint", disc);
    }
  },
};

/**
 * Wire codec for `CreateTableSchema` (0 data words, 10 pointers).
 *
 * @internal
 */
export const CreateTableSchemaCodec: StructCodec<T.CreateTableSchema> = {
  dataWords: 0,
  pointerCount: 10,
  write(s, v, caps) {
    s.setText(0, v.table ?? "");
    {
      const list = v.columns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        NamedSqlTypeCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setText(2, v.identityColumn ?? "");
    s.setBoolList(3, v.columnNotNull ?? []);
    s.setBoolList(4, v.columnUnique ?? []);
    s.setBoolList(5, v.columnPrimaryKey ?? []);
    s.setTextList(6, v.columnDefaults ?? []);
    s.setTextList(7, v.columnChecks ?? []);
    {
      const list = v.columnReferences ?? [];
      const items = s.initStructList(8, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        OptionalColumnReferenceCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.tableConstraints ?? [];
      const items = s.initStructList(9, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        TableConstraintCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      table: s.getText(0),
      columns: s.getStructList(1, 1, 1).map((item) => NamedSqlTypeCodec.read(item, caps)),
      identityColumn: s.getText(2),
      columnNotNull: s.getBoolList(3),
      columnUnique: s.getBoolList(4),
      columnPrimaryKey: s.getBoolList(5),
      columnDefaults: s.getTextList(6),
      columnChecks: s.getTextList(7),
      columnReferences: s.getStructList(8, 1, 1).map((item) => OptionalColumnReferenceCodec.read(item, caps)),
      tableConstraints: s.getStructList(9, 1, 1).map((item) => TableConstraintCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `SchemaCreate` (1 data words, 2 pointers).
 *
 * @internal
 */
export const SchemaCreateCodec: StructCodec<T.SchemaCreate> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.schema != null) {
      CreateTableSchemaCodec.write(s.initStruct(0, 0, 10), v.schema, caps);
    }
    s.setText(1, v.fingerprint ?? "");
    s.setBool(0, v.noop ?? false);
  },
  read(s, caps) {
    return {
      schema: CreateTableSchemaCodec.read(s.getStruct(0, 0, 10), caps),
      fingerprint: s.getText(1),
      noop: s.getBool(0),
    };
  },
};

/**
 * Wire codec for `SchemaAction` (1 data words, 1 pointers).
 *
 * @internal
 */
export const SchemaActionCodec: StructCodec<T.SchemaAction> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "none":
        s.setUint16(0, 0);
        break;
      case "create":
        s.setUint16(0, 1);
        if (v.value != null) {
          SchemaCreateCodec.write(s.initStruct(0, 1, 2), v.value, caps);
        }
        break;
      case "drop":
        s.setUint16(0, 2);
        s.setText(0, v.value ?? "");
        break;
      default:
        throw unknownUnion("SchemaAction", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "none" };
      case 1:
        return { kind: "create", value: SchemaCreateCodec.read(s.getStruct(0, 1, 2), caps) };
      case 2:
        return { kind: "drop", value: s.getText(0) };
      default:
        throw unknownUnion("SchemaAction", disc);
    }
  },
};

/**
 * Wire codec for `ResolvedStatement` (0 data words, 8 pointers).
 *
 * @internal
 */
export const ResolvedStatementCodec: StructCodec<T.ResolvedStatement> = {
  dataWords: 0,
  pointerCount: 8,
  write(s, v, caps) {
    s.setText(0, v.statementHash ?? "");
    {
      const list = v.outputColumns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        NamedSqlTypeCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.physicalAccesses ?? [];
      const items = s.initStructList(2, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        PhysicalAccessCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.assignments ?? [];
      const items = s.initStructList(3, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        ResolvedAssignmentCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.textCollateSites ?? [];
      const items = s.initStructList(4, list.length, 0, 1);
      for (let i = 0; i < items.length; i++) {
        TextCollateSiteCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.integerArithSites ?? [];
      const items = s.initStructList(5, list.length, 1, 3);
      for (let i = 0; i < items.length; i++) {
        IntegerArithSiteCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setTextList(6, v.functions ?? []);
    if (v.schemaAction != null) {
      SchemaActionCodec.write(s.initStruct(7, 1, 1), v.schemaAction, caps);
    }
  },
  read(s, caps) {
    return {
      statementHash: s.getText(0),
      outputColumns: s.getStructList(1, 1, 1).map((item) => NamedSqlTypeCodec.read(item, caps)),
      physicalAccesses: s.getStructList(2, 0, 2).map((item) => PhysicalAccessCodec.read(item, caps)),
      assignments: s.getStructList(3, 1, 2).map((item) => ResolvedAssignmentCodec.read(item, caps)),
      textCollateSites: s.getStructList(4, 0, 1).map((item) => TextCollateSiteCodec.read(item, caps)),
      integerArithSites: s.getStructList(5, 1, 3).map((item) => IntegerArithSiteCodec.read(item, caps)),
      functions: s.getTextList(6),
      schemaAction: SchemaActionCodec.read(s.getStruct(7, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for `AdapterReceipt` (1 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterReceiptCodec: StructCodec<T.AdapterReceipt> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.guestLen ?? 0);
    s.setText(0, v.guestHash ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      guestLen: s.getUint32(0),
      guestHash: s.getText(0),
    };
  },
};

/**
 * Wire codec for `AdapterStatement` (1 data words, 3 pointers).
 *
 * @internal
 */
export const AdapterStatementCodec: StructCodec<T.AdapterStatement> = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.sql ?? "");
    {
      const list = v.parameters ?? [];
      const items = s.initStructList(1, list.length, 2, 1);
      for (let i = 0; i < items.length; i++) {
        DbValueCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setUint16(0, ord(A.DB_STATEMENT_KINDS, v.kind ?? A.DB_STATEMENT_KINDS[0]!, "DbStatementKind"));
    s.setUint32(1, v.maxRows ?? 0);
    s.setUint16(1, ord(A.DB_RESULT_SELECTIONS, v.resultSelection ?? A.DB_RESULT_SELECTIONS[0]!, "DbResultSelection"));
    if (v.proof != null) {
      ResolvedStatementCodec.write(s.initStruct(2, 0, 8), v.proof, caps);
    }
  },
  read(s, caps) {
    return {
      sql: s.getText(0),
      parameters: s.getStructList(1, 2, 1).map((item) => DbValueCodec.read(item, caps)),
      kind: fromOrd(A.DB_STATEMENT_KINDS, s.getUint16(0), "DbStatementKind"),
      maxRows: s.getUint32(1),
      resultSelection: fromOrd(A.DB_RESULT_SELECTIONS, s.getUint16(1), "DbResultSelection"),
      proof: ResolvedStatementCodec.read(s.getStruct(2, 0, 8), caps),
    };
  },
};

/**
 * Wire codec for `AdapterExecuteRequest` (2 data words, 4 pointers).
 *
 * @internal
 */
export const AdapterExecuteRequestCodec: StructCodec<T.AdapterExecuteRequest> = {
  dataWords: 2,
  pointerCount: 4,
  write(s, v, caps) {
    s.setText(0, v.operationId ?? "");
    s.setText(1, v.requestHash ?? "");
    {
      const list = v.statements ?? [];
      const items = s.initStructList(2, list.length, 1, 3);
      for (let i = 0; i < items.length; i++) {
        AdapterStatementCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setUint64(0, BigInt(v.deadlineUnixMs ?? 0));
    s.setUint16(4, ord(A.ISOLATION_REQS, v.isolation ?? A.ISOLATION_REQS[0]!, "IsolationReq"));
    if (v.receipt != null) {
      AdapterReceiptCodec.write(s.initStruct(3, 1, 1), v.receipt, caps);
    }
  },
  read(s, caps) {
    return {
      operationId: s.getText(0),
      requestHash: s.getText(1),
      statements: s.getStructList(2, 1, 3).map((item) => AdapterStatementCodec.read(item, caps)),
      deadlineUnixMs: Number(s.getUint64(0)),
      isolation: fromOrd(A.ISOLATION_REQS, s.getUint16(4), "IsolationReq"),
      receipt: AdapterReceiptCodec.read(s.getStruct(3, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for `StatementResult` (1 data words, 2 pointers).
 *
 * @internal
 */
export const StatementResultCodec: StructCodec<T.StatementResult> = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.rows ?? [];
      const items = s.initStructList(0, list.length, 0, 1);
      for (let i = 0; i < items.length; i++) {
        DbRowCodec.write(items[i]!, list[i]!, caps);
      }
    }
    {
      const list = v.columns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        DbColumnCodec.write(items[i]!, list[i]!, caps);
      }
    }
    s.setUint64(0, BigInt(v.rowsAffected ?? 0));
  },
  read(s, caps) {
    return {
      rows: s.getStructList(0, 0, 1).map((item) => DbRowCodec.read(item, caps)),
      columns: s.getStructList(1, 1, 1).map((item) => DbColumnCodec.read(item, caps)),
      rowsAffected: Number(s.getUint64(0)),
    };
  },
};

/**
 * Wire codec for `DbTiming` (2 data words, 1 pointers).
 *
 * @internal
 */
export const DbTimingCodec: StructCodec<T.DbTiming> = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.attemptElapsedUs ?? 0));
    s.setUint64(1, BigInt(v.dbExecutionUs ?? 0));
    s.setText(0, v.dbTimingSource ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      attemptElapsedUs: Number(s.getUint64(0)),
      dbExecutionUs: Number(s.getUint64(1)),
      dbTimingSource: s.getText(0),
    };
  },
};

/**
 * Wire codec for `ExecuteReply` (0 data words, 3 pointers).
 *
 * @internal
 */
export const ExecuteReplyCodec: StructCodec<T.ExecuteReply> = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.operationId ?? "");
    {
      const list = v.statements ?? [];
      const items = s.initStructList(1, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        StatementResultCodec.write(items[i]!, list[i]!, caps);
      }
    }
    if (v.timing != null) {
      DbTimingCodec.write(s.initStruct(2, 2, 1), v.timing, caps);
    }
  },
  read(s, caps) {
    return {
      operationId: s.getText(0),
      statements: s.getStructList(1, 1, 2).map((item) => StatementResultCodec.read(item, caps)),
      timing: DbTimingCodec.read(s.getStruct(2, 2, 1), caps),
    };
  },
};

/**
 * Wire codec for `ExecuteResultReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const ExecuteResultReplyCodec: StructCodec<T.ExecuteResultReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ExecuteReplyCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExecuteResultReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ExecuteReplyCodec.read(s.getStruct(0, 0, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ExecuteResultReply", disc);
    }
  },
};

/**
 * Wire codec for `DbCapabilities` (7 data words, 0 pointers).
 *
 * @internal
 */
export const DbCapabilitiesCodec: StructCodec<T.DbCapabilities> = {
  dataWords: 7,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.sqlContractVersion ?? 0);
    s.setBool(32, v.atomicBatch ?? false);
    s.setBool(33, v.returning ?? false);
    s.setBool(34, v.affectedRows ?? false);
    s.setBool(35, v.schemaMigrations ?? false);
    s.setBool(36, v.cancellation ?? false);
    s.setBool(37, v.timing ?? false);
    s.setUint32(2, v.maxBinds ?? 0);
    s.setUint32(3, v.maxStatements ?? 0);
    s.setUint32(4, v.maxResultRows ?? 0);
    s.setUint32(5, v.maxPayloadBytes ?? 0);
    s.setUint32(6, v.maxResultBytes ?? 0);
    s.setUint32(7, v.maxCellBytes ?? 0);
    s.setUint32(8, v.maxRequestBytes ?? 0);
    s.setUint32(9, v.maxAtomicResultBytes ?? 0);
    s.setBool(38, v.pluginDatabases ?? false);
    s.setUint32(10, v.maxFunctionArgs ?? 0);
    s.setUint32(11, v.maxSchemaColumns ?? 0);
    s.setUint32(12, v.maxPatternBytes ?? 0);
    s.setUint32(13, v.maxLoweredStatementBytes ?? 0);
    s.setBool(39, v.consistentBackupRead ?? false);
    s.setBool(40, v.atomicUnitRestore ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      sqlContractVersion: s.getUint32(0),
      atomicBatch: s.getBool(32),
      returning: s.getBool(33),
      affectedRows: s.getBool(34),
      schemaMigrations: s.getBool(35),
      cancellation: s.getBool(36),
      timing: s.getBool(37),
      maxBinds: s.getUint32(2),
      maxStatements: s.getUint32(3),
      maxResultRows: s.getUint32(4),
      maxPayloadBytes: s.getUint32(5),
      maxResultBytes: s.getUint32(6),
      maxCellBytes: s.getUint32(7),
      maxRequestBytes: s.getUint32(8),
      maxAtomicResultBytes: s.getUint32(9),
      pluginDatabases: s.getBool(38),
      maxFunctionArgs: s.getUint32(10),
      maxSchemaColumns: s.getUint32(11),
      maxPatternBytes: s.getUint32(12),
      maxLoweredStatementBytes: s.getUint32(13),
      consistentBackupRead: s.getBool(39),
      atomicUnitRestore: s.getBool(40),
    };
  },
};

/**
 * Wire codec for `DbBootstrapReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const DbBootstrapReplyCodec: StructCodec<T.DbBootstrapReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DbBootstrapCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DbBootstrapReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DbBootstrapCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DbBootstrapReply", disc);
    }
  },
};

/**
 * Wire codec for `DbBootstrap` (0 data words, 1 pointers).
 *
 * @internal
 */
export const DbBootstrapCodec: StructCodec<T.DbBootstrap> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.engine ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      engine: s.getText(0),
    };
  },
};

/**
 * Wire codec for `DbCapabilitiesReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const DbCapabilitiesReplyCodec: StructCodec<T.DbCapabilitiesReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DbCapabilitiesCodec.write(s.initStruct(0, 7, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DbCapabilitiesReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DbCapabilitiesCodec.read(s.getStruct(0, 7, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DbCapabilitiesReply", disc);
    }
  },
};

/**
 * Wire codec for `IdentityHighWater` (1 data words, 1 pointers).
 *
 * @internal
 */
export const IdentityHighWaterCodec: StructCodec<T.IdentityHighWater> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setInt64(0, v.last ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      last: s.getInt64(0),
    };
  },
};

/**
 * Wire codec for `IdentityExportReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const IdentityExportReplyCodec: StructCodec<T.IdentityExportReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 1, 1);
          for (let i = 0; i < items.length; i++) {
            IdentityHighWaterCodec.write(items[i]!, list[i]!, caps);
          }
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("IdentityExportReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getStructList(0, 1, 1).map((item) => IdentityHighWaterCodec.read(item, caps)) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("IdentityExportReply", disc);
    }
  },
};

/**
 * Wire codec for `UserRelationsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const UserRelationsReplyCodec: StructCodec<T.UserRelationsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        s.setTextList(0, v.value ?? []);
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("UserRelationsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getTextList(0) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("UserRelationsReply", disc);
    }
  },
};

/**
 * Wire codec for `PluginMigrationOp` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PluginMigrationOpCodec: StructCodec<T.PluginMigrationOp> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    switch (v.kind) {
      case "schema":
        s.setUint16(0, 0);
        s.setText(0, v.value ?? "");
        break;
      case "data":
        s.setUint16(0, 1);
        s.setText(0, v.value ?? "");
        break;
      default:
        throw unknownUnion("PluginMigrationOp", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    void caps;
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "schema", value: s.getText(0) };
      case 1:
        return { kind: "data", value: s.getText(0) };
      default:
        throw unknownUnion("PluginMigrationOp", disc);
    }
  },
};

/**
 * Wire codec for `PluginMigration` (0 data words, 2 pointers).
 *
 * @internal
 */
export const PluginMigrationCodec: StructCodec<T.PluginMigration> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.id ?? "");
    {
      const list = v.operations ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        PluginMigrationOpCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      id: s.getText(0),
      operations: s.getStructList(1, 1, 1).map((item) => PluginMigrationOpCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `PluginMigrationsOk` (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginMigrationsOkCodec: StructCodec<T.PluginMigrationsOk> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.migrations ?? [];
      const items = s.initStructList(0, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        PluginMigrationCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      migrations: s.getStructList(0, 0, 2).map((item) => PluginMigrationCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for `PluginMigrationsReply` (1 data words, 1 pointers).
 *
 * @internal
 */
export const PluginMigrationsReplyCodec: StructCodec<T.PluginMigrationsReply> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PluginMigrationsOkCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PluginMigrationsReply", (v as { kind: string }).kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PluginMigrationsOkCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PluginMigrationsReply", disc);
    }
  },
};

/**
 * Wire codec for the `ByteSource.pull` params envelope (1 data words, 0 pointers).
 *
 * @internal
 */
export const ByteSourcePullParamsCodec: StructCodec<ByteSourcePullParams> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.maxBytes ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      maxBytes: s.getUint32(0),
    };
  },
};

/**
 * Wire codec for the `ByteSource.pull` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ByteSourcePullResultsCodec: StructCodec<ByteSourcePullResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PullReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PullReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.head` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationHeadParamsCodec: StructCodec<DestinationHeadParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
    };
  },
};

/**
 * Wire codec for the `Destination.head` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationHeadResultsCodec: StructCodec<DestinationHeadResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HeadReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HeadReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.list` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationListParamsCodec: StructCodec<DestinationListParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.options != null) {
      ListOptionsCodec.write(s.initStruct(0, 1, 2), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      options: ListOptionsCodec.read(s.getStruct(0, 1, 2), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.list` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationListResultsCodec: StructCodec<DestinationListResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ListReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ListReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.get` params envelope (0 data words, 2 pointers).
 *
 * @internal
 */
export const DestinationGetParamsCodec: StructCodec<DestinationGetParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.key ?? "");
    if (v.options != null) {
      ReadOptionsCodec.write(s.initStruct(1, 0, 1), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      key: s.getText(0),
      options: ReadOptionsCodec.read(s.getStruct(1, 0, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.get` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationGetResultsCodec: StructCodec<DestinationGetResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      GetReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: GetReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.put` params envelope (0 data words, 3 pointers).
 *
 * @internal
 */
export const DestinationPutParamsCodec: StructCodec<DestinationPutParams> = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.key ?? "");
    s.setCap(1, caps.exportCap(v.body));
    if (v.options != null) {
      WriteOptionsCodec.write(s.initStruct(2, 2, 3), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      key: s.getText(0),
      body: caps.importCap(s.getCapIndex(1)) as T.ByteSource,
      options: WriteOptionsCodec.read(s.getStruct(2, 2, 3), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.put` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationPutResultsCodec: StructCodec<DestinationPutResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PutReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PutReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.copy` params envelope (0 data words, 2 pointers).
 *
 * @internal
 */
export const DestinationCopyParamsCodec: StructCodec<DestinationCopyParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.from ?? "");
    s.setText(1, v.to ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      from: s.getText(0),
      to: s.getText(1),
    };
  },
};

/**
 * Wire codec for the `Destination.copy` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationCopyResultsCodec: StructCodec<DestinationCopyResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CopyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CopyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.delete` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationDeleteParamsCodec: StructCodec<DestinationDeleteParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
    };
  },
};

/**
 * Wire codec for the `Destination.delete` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationDeleteResultsCodec: StructCodec<DestinationDeleteResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.commit` params envelope (0 data words, 2 pointers).
 *
 * @internal
 */
export const DestinationCommitParamsCodec: StructCodec<DestinationCommitParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setText(1, v.commitToken ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      commitToken: s.getText(1),
    };
  },
};

/**
 * Wire codec for the `Destination.commit` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationCommitResultsCodec: StructCodec<DestinationCommitResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PutReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PutReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Destination.abortStage` params envelope (0 data words, 2 pointers).
 *
 * @internal
 */
export const DestinationAbortStageParamsCodec: StructCodec<DestinationAbortStageParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setText(1, v.commitToken ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      commitToken: s.getText(1),
    };
  },
};

/**
 * Wire codec for the `Destination.abortStage` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DestinationAbortStageResultsCodec: StructCodec<DestinationAbortStageResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Source.open` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const SourceOpenParamsCodec: StructCodec<SourceOpenParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
    };
  },
};

/**
 * Wire codec for the `Source.open` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const SourceOpenResultsCodec: StructCodec<SourceOpenResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      OpenReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: OpenReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ProgressSink.report` params envelope (1 data words, 1 pointers).
 *
 * @internal
 */
export const ProgressSinkReportParamsCodec: StructCodec<ProgressSinkReportParams> = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setFloat32(0, v.percent ?? 0);
    s.setText(0, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      percent: s.getFloat32(0),
      message: s.getText(0),
    };
  },
};

/**
 * Wire codec for the `ProgressSink.report` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ProgressSinkReportResultsCodec: StructCodec<ProgressSinkReportResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Cancellation.poll` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const CancellationPollParamsCodec: StructCodec<CancellationPollParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `Cancellation.poll` results envelope (1 data words, 0 pointers).
 *
 * @internal
 */
export const CancellationPollResultsCodec: StructCodec<CancellationPollResults> = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.cancelled ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      cancelled: s.getBool(0),
    };
  },
};

/**
 * Wire codec for the `JobRunner.job` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const JobRunnerJobParamsCodec: StructCodec<JobRunnerJobParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.controller != null) {
      JobControllerCodec.write(s.initStruct(0, 0, 5), v.controller, caps);
    }
  },
  read(s, caps) {
    return {
      controller: JobControllerCodec.read(s.getStruct(0, 0, 5), caps),
    };
  },
};

/**
 * Wire codec for the `JobRunner.job` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const JobRunnerJobResultsCodec: StructCodec<JobRunnerJobResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HandleReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HandleReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `EventConsumer.event` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventConsumerEventParamsCodec: StructCodec<EventConsumerEventParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.batch != null) {
      EventBatchCodec.write(s.initStruct(0, 0, 1), v.batch, caps);
    }
  },
  read(s, caps) {
    return {
      batch: EventBatchCodec.read(s.getStruct(0, 0, 1), caps),
    };
  },
};

/**
 * Wire codec for the `EventConsumer.event` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventConsumerEventResultsCodec: StructCodec<EventConsumerEventResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EventBatchReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EventBatchReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `EventPublisher.publish` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventPublisherPublishParamsCodec: StructCodec<EventPublisherPublishParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.event != null) {
      PluginEventCodec.write(s.initStruct(0, 2, 5), v.event, caps);
    }
  },
  read(s, caps) {
    return {
      event: PluginEventCodec.read(s.getStruct(0, 2, 5), caps),
    };
  },
};

/**
 * Wire codec for the `EventPublisher.publish` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const EventPublisherPublishResultsCodec: StructCodec<EventPublisherPublishResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PublishReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PublishReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.login` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginParamsCodec: StructCodec<ContentSourceLoginParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginParamsCodec.write(s.initStruct(0, 2, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginParamsCodec.read(s.getStruct(0, 2, 10), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.login` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginResultsCodec: StructCodec<ContentSourceLoginResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.scan` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceScanParamsCodec: StructCodec<ContentSourceScanParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ScanParamsCodec.write(s.initStruct(0, 1, 3), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ScanParamsCodec.read(s.getStruct(0, 1, 3), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.scan` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceScanResultsCodec: StructCodec<ContentSourceScanResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ScanReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ScanReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.fetchTitle` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceFetchTitleParamsCodec: StructCodec<ContentSourceFetchTitleParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      FetchTitleParamsCodec.write(s.initStruct(0, 0, 7), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: FetchTitleParamsCodec.read(s.getStruct(0, 0, 7), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.fetchTitle` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceFetchTitleResultsCodec: StructCodec<ContentSourceFetchTitleResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      FetchTitleReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: FetchTitleReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.listAccounts` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const ContentSourceListAccountsParamsCodec: StructCodec<ContentSourceListAccountsParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `ContentSource.listAccounts` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceListAccountsResultsCodec: StructCodec<ContentSourceListAccountsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      SourceAccountsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: SourceAccountsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.loginStart` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginStartParamsCodec: StructCodec<ContentSourceLoginStartParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginParamsCodec.write(s.initStruct(0, 2, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginParamsCodec.read(s.getStruct(0, 2, 10), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.loginStart` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginStartResultsCodec: StructCodec<ContentSourceLoginStartResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginStartReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginStartReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.loginComplete` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginCompleteParamsCodec: StructCodec<ContentSourceLoginCompleteParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginCompleteParamsCodec.write(s.initStruct(0, 0, 1), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginCompleteParamsCodec.read(s.getStruct(0, 0, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.loginComplete` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceLoginCompleteResultsCodec: StructCodec<ContentSourceLoginCompleteResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.searchCatalog` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceSearchCatalogParamsCodec: StructCodec<ContentSourceSearchCatalogParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      SearchCatalogParamsCodec.write(s.initStruct(0, 2, 3), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: SearchCatalogParamsCodec.read(s.getStruct(0, 2, 3), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.searchCatalog` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceSearchCatalogResultsCodec: StructCodec<ContentSourceSearchCatalogResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.expandCandidates` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceExpandCandidatesParamsCodec: StructCodec<ContentSourceExpandCandidatesParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ExpandCandidatesParamsCodec.write(s.initStruct(0, 1, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ExpandCandidatesParamsCodec.read(s.getStruct(0, 1, 10), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.expandCandidates` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceExpandCandidatesResultsCodec: StructCodec<ContentSourceExpandCandidatesResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.purchaseHint` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourcePurchaseHintParamsCodec: StructCodec<ContentSourcePurchaseHintParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      PurchaseHintParamsCodec.write(s.initStruct(0, 1, 6), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: PurchaseHintParamsCodec.read(s.getStruct(0, 1, 6), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.purchaseHint` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourcePurchaseHintResultsCodec: StructCodec<ContentSourcePurchaseHintResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PurchaseHintReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PurchaseHintReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.listDeals` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceListDealsParamsCodec: StructCodec<ContentSourceListDealsParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ListDealsParamsCodec.write(s.initStruct(0, 1, 0), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ListDealsParamsCodec.read(s.getStruct(0, 1, 0), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.listDeals` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceListDealsResultsCodec: StructCodec<ContentSourceListDealsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.health` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const ContentSourceHealthParamsCodec: StructCodec<ContentSourceHealthParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `ContentSource.health` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceHealthResultsCodec: StructCodec<ContentSourceHealthResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HealthReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HealthReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.diagnose` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const ContentSourceDiagnoseParamsCodec: StructCodec<ContentSourceDiagnoseParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `ContentSource.diagnose` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceDiagnoseResultsCodec: StructCodec<ContentSourceDiagnoseResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DiagnoseReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DiagnoseReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.catalogDetail` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceCatalogDetailParamsCodec: StructCodec<ContentSourceCatalogDetailParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      CatalogDetailParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: CatalogDetailParamsCodec.read(s.getStruct(0, 0, 2), caps),
    };
  },
};

/**
 * Wire codec for the `ContentSource.catalogDetail` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const ContentSourceCatalogDetailResultsCodec: StructCodec<ContentSourceCatalogDetailResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogDetailReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogDetailReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.health` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibraryHealthParamsCodec: StructCodec<RemoteLibraryHealthParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.health` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryHealthResultsCodec: StructCodec<RemoteLibraryHealthResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HealthReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HealthReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.start` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibraryStartParamsCodec: StructCodec<RemoteLibraryStartParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.start` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryStartResultsCodec: StructCodec<RemoteLibraryStartResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.stop` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibraryStopParamsCodec: StructCodec<RemoteLibraryStopParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.stop` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryStopResultsCodec: StructCodec<RemoteLibraryStopResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.diagnose` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibraryDiagnoseParamsCodec: StructCodec<RemoteLibraryDiagnoseParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.diagnose` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryDiagnoseResultsCodec: StructCodec<RemoteLibraryDiagnoseResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DiagnoseReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DiagnoseReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.scanLibrary` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryScanLibraryParamsCodec: StructCodec<RemoteLibraryScanLibraryParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ScanLibraryParamsCodec.write(s.initStruct(0, 1, 0), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ScanLibraryParamsCodec.read(s.getStruct(0, 1, 0), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.scanLibrary` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryScanLibraryResultsCodec: StructCodec<RemoteLibraryScanLibraryResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.syncListening` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibrarySyncListeningParamsCodec: StructCodec<RemoteLibrarySyncListeningParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.syncListening` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibrarySyncListeningResultsCodec: StructCodec<RemoteLibrarySyncListeningResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      SyncListeningReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: SyncListeningReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `RemoteLibrary.pollEvents` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const RemoteLibraryPollEventsParamsCodec: StructCodec<RemoteLibraryPollEventsParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `RemoteLibrary.pollEvents` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const RemoteLibraryPollEventsResultsCodec: StructCodec<RemoteLibraryPollEventsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EventPollReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EventPollReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginCli.describe` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const PluginCliDescribeParamsCodec: StructCodec<PluginCliDescribeParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `PluginCli.describe` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginCliDescribeResultsCodec: StructCodec<PluginCliDescribeResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CliSchemaReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CliSchemaReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginCli.invoke` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginCliInvokeParamsCodec: StructCodec<PluginCliInvokeParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      CliInvokeParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: CliInvokeParamsCodec.read(s.getStruct(0, 0, 2), caps),
    };
  },
};

/**
 * Wire codec for the `PluginCli.invoke` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginCliInvokeResultsCodec: StructCodec<PluginCliInvokeResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CliInvokeReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CliInvokeReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Oidc.clients` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const OidcClientsParamsCodec: StructCodec<OidcClientsParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `Oidc.clients` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const OidcClientsResultsCodec: StructCodec<OidcClientsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      OidcClientsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: OidcClientsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Oidc.authenticateUser` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const OidcAuthenticateUserParamsCodec: StructCodec<OidcAuthenticateUserParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      AuthenticateUserParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: AuthenticateUserParamsCodec.read(s.getStruct(0, 0, 2), caps),
    };
  },
};

/**
 * Wire codec for the `Oidc.authenticateUser` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const OidcAuthenticateUserResultsCodec: StructCodec<OidcAuthenticateUserResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExternalUserReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExternalUserReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `Database.openSession` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const DatabaseOpenSessionParamsCodec: StructCodec<DatabaseOpenSessionParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `Database.openSession` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const DatabaseOpenSessionResultsCodec: StructCodec<DatabaseOpenSessionResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      AdapterSessionReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: AdapterSessionReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.capabilities` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionCapabilitiesParamsCodec: StructCodec<AdapterDatabaseSessionCapabilitiesParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.capabilities` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionCapabilitiesResultsCodec: StructCodec<AdapterDatabaseSessionCapabilitiesResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DbCapabilitiesReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DbCapabilitiesReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.execute` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionExecuteParamsCodec: StructCodec<AdapterDatabaseSessionExecuteParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.request != null) {
      AdapterExecuteRequestCodec.write(s.initStruct(0, 2, 4), v.request, caps);
    }
  },
  read(s, caps) {
    return {
      request: AdapterExecuteRequestCodec.read(s.getStruct(0, 2, 4), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.execute` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionExecuteResultsCodec: StructCodec<AdapterDatabaseSessionExecuteResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExecuteResultReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExecuteResultReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.close` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionCloseParamsCodec: StructCodec<AdapterDatabaseSessionCloseParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.close` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionCloseResultsCodec: StructCodec<AdapterDatabaseSessionCloseResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.bootstrap` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionBootstrapParamsCodec: StructCodec<AdapterDatabaseSessionBootstrapParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.bootstrap` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionBootstrapResultsCodec: StructCodec<AdapterDatabaseSessionBootstrapResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DbBootstrapReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DbBootstrapReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.exportIdentity` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionExportIdentityParamsCodec: StructCodec<AdapterDatabaseSessionExportIdentityParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.exportIdentity` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionExportIdentityResultsCodec: StructCodec<AdapterDatabaseSessionExportIdentityResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      IdentityExportReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: IdentityExportReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.importIdentity` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionImportIdentityParamsCodec: StructCodec<AdapterDatabaseSessionImportIdentityParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.rows ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        IdentityHighWaterCodec.write(items[i]!, list[i]!, caps);
      }
    }
  },
  read(s, caps) {
    return {
      rows: s.getStructList(0, 1, 1).map((item) => IdentityHighWaterCodec.read(item, caps)),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.importIdentity` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionImportIdentityResultsCodec: StructCodec<AdapterDatabaseSessionImportIdentityResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.listUserRelations` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionListUserRelationsParamsCodec: StructCodec<AdapterDatabaseSessionListUserRelationsParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.listUserRelations` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionListUserRelationsResultsCodec: StructCodec<AdapterDatabaseSessionListUserRelationsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      UserRelationsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: UserRelationsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.prepareUnitRestore` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionPrepareUnitRestoreParamsCodec: StructCodec<AdapterDatabaseSessionPrepareUnitRestoreParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.prepareUnitRestore` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionPrepareUnitRestoreResultsCodec: StructCodec<AdapterDatabaseSessionPrepareUnitRestoreResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.dropUserRelations` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionDropUserRelationsParamsCodec: StructCodec<AdapterDatabaseSessionDropUserRelationsParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.names ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      names: s.getTextList(0),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.dropUserRelations` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionDropUserRelationsResultsCodec: StructCodec<AdapterDatabaseSessionDropUserRelationsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.assertRestoreConstraints` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionAssertRestoreConstraintsParamsCodec: StructCodec<AdapterDatabaseSessionAssertRestoreConstraintsParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `AdapterDatabaseSession.assertRestoreConstraints` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const AdapterDatabaseSessionAssertRestoreConstraintsResultsCodec: StructCodec<AdapterDatabaseSessionAssertRestoreConstraintsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `GuestDatabase.execute` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const GuestDatabaseExecuteParamsCodec: StructCodec<GuestDatabaseExecuteParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.request != null) {
      ExecuteRequestCodec.write(s.initStruct(0, 1, 3), v.request, caps);
    }
  },
  read(s, caps) {
    return {
      request: ExecuteRequestCodec.read(s.getStruct(0, 1, 3), caps),
    };
  },
};

/**
 * Wire codec for the `GuestDatabase.execute` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const GuestDatabaseExecuteResultsCodec: StructCodec<GuestDatabaseExecuteResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExecuteResultReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExecuteResultReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `GuestDatabase.close` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const GuestDatabaseCloseParamsCodec: StructCodec<GuestDatabaseCloseParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `GuestDatabase.close` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const GuestDatabaseCloseResultsCodec: StructCodec<GuestDatabaseCloseResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.describe` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const PluginWorkerDescribeParamsCodec: StructCodec<PluginWorkerDescribeParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `PluginWorker.describe` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginWorkerDescribeResultsCodec: StructCodec<PluginWorkerDescribeResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DescribeReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DescribeReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.open` params envelope (0 data words, 2 pointers).
 *
 * @internal
 */
export const PluginWorkerOpenParamsCodec: StructCodec<PluginWorkerOpenParams> = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.invocation != null) {
      InvocationCodec.write(s.initStruct(0, 1, 4), v.invocation, caps);
    }
    if (v.bindings != null) {
      BindingsCodec.write(s.initStruct(1, 0, 7), v.bindings, caps);
    }
  },
  read(s, caps) {
    return {
      invocation: InvocationCodec.read(s.getStruct(0, 1, 4), caps),
      bindings: BindingsCodec.read(s.getStruct(1, 0, 7), caps),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.open` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginWorkerOpenResultsCodec: StructCodec<PluginWorkerOpenResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EntrypointsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EntrypointsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.shutdown` params envelope (0 data words, 0 pointers).
 *
 * @internal
 */
export const PluginWorkerShutdownParamsCodec: StructCodec<PluginWorkerShutdownParams> = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  },
};

/**
 * Wire codec for the `PluginWorker.shutdown` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginWorkerShutdownResultsCodec: StructCodec<PluginWorkerShutdownResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.databaseMigrations` params envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginWorkerDatabaseMigrationsParamsCodec: StructCodec<PluginWorkerDatabaseMigrationsParams> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.binding ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      binding: s.getText(0),
    };
  },
};

/**
 * Wire codec for the `PluginWorker.databaseMigrations` results envelope (0 data words, 1 pointers).
 *
 * @internal
 */
export const PluginWorkerDatabaseMigrationsResultsCodec: StructCodec<PluginWorkerDatabaseMigrationsResults> = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PluginMigrationsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PluginMigrationsReplyCodec.read(s.getStruct(0, 1, 1), caps),
    };
  },
};
