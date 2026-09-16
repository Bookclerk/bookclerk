# Bookclerk host-private plugin ABI extensions (`api_version = 2`).
#
# Not part of the public plugin author contract. Host ↔ first-party database
# adapter guests only (SeaORM interactive transaction proxy).
@0xb7c4e2f91a0835d2;

using Plugin = import "plugin.capnp";

struct AdapterTransactionReply {
  union {
    ok @0 :AdapterTransaction;
    err @1 :Plugin.PluginError;
  }
}

interface AdapterTransaction {
  execute @0 (request :Plugin.ExecuteRequest) -> (result :Plugin.ExecuteResultReply);
  commit @1 () -> (result :Plugin.EmptyReply);
  rollback @2 () -> (result :Plugin.EmptyReply);
  executeEnvelope @3 (envelope :HostExecuteEnvelope) -> (result :Plugin.ExecuteResultReply);
}

struct HostGuestReceiptPersist {
  guestLen @0 :UInt32;
  guestHash @1 :Text;
}

struct HostExecuteEnvelope {
  request @0 :Plugin.ExecuteRequest;
  guestReceipt @1 :HostGuestReceiptPersist;
  # Host-private JSON of Vec<ResolvedStatement> (not on public ExecuteRequest).
  proofsJson @2 :Text;
}

interface HostAdapterDatabaseSession {
  begin @0 () -> (result :AdapterTransactionReply);
  executeEnvelope @1 (envelope :HostExecuteEnvelope) -> (result :Plugin.ExecuteResultReply);
}
