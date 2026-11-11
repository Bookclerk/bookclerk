using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
  ],
  sockets = [
    (name = "rpc", address = "127.0.0.1:4321", http = (), service = "bridge")
  ]
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "2026-08-01",
  
  bindings = [
    (name = "EGRESS_POLICY", json = "{\"mode\":\"deny\",\"domains\":[],\"maxRedirects\":10,\"ownUserAgentHosts\":[]}")
  ],
  globalOutbound = "internet",
);

const pluginWorker :Workerd.Worker = (
  modules = [
    (name = "index.js", esModule = embed "/modules/index.js"),
    (name = "@bookclerk/plugin-sdk/workerd", esModule = embed ".bookclerk/sdk-workerd.js"),
    (name = "@bookclerk/plugin-sdk", esModule = embed ".bookclerk/sdk-workerd.js")
  ],
  compatibilityDate = "2026-08-01",
  
  bindings = [],
  globalOutbound = "blocked",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    (name = "adapter.js", esModule = embed ".bookclerk/adapter.js"),
    (name = "@bookclerk/plugin-sdk/workerd", esModule = embed ".bookclerk/sdk-workerd.js"),
    (name = "@bookclerk/plugin-sdk", esModule = embed ".bookclerk/sdk-workerd.js")
  ],
  compatibilityDate = "2026-08-01",
  
  bindings = [
    (name = "PLUGIN", service = "plugin"),
    (name = "PLUGIN_CLI", service = (name = "plugin", entrypoint = "Cli")),
    (name = "PLUGIN_DESCRIBE", json = "{\"apiVersion\":3,\"id\":\"echo_workerd_tools\",\"displayName\":\"\",\"rpcFeatures\":[\"rpc.scalarLimits\",\"rpc.streams\"],\"capabilities\":{\"entrypoints\":[\"cli\"],\"consumes\":[],\"produces\":[],\"jobs\":[],\"databases\":[],\"bindings\":[\"CONFIG\"],\"queueConsumers\":[],\"queueProducers\":[],\"crons\":[]},\"cli\":{\"commands\":[]}}"),
    (name = "BRIDGE_TOKEN", text = "tok\"en")
  ],
  globalOutbound = "blocked",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "2026-08-01",
  
  bindings = [
    (name = "PLUGIN", service = "adapter"),
    (name = "BRIDGE_TOKEN", text = "tok\"en")
  ],
  globalOutbound = "blocked",
);
