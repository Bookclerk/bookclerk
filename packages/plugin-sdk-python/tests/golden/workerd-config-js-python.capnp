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
    (name = "EGRESS_POLICY", json = "{\"mode\":\"outbound\",\"domains\":[\"api.example.com\",\"cdn.jsdelivr.net\",\"pypi.org\",\"files.pythonhosted.org\"],\"maxRedirects\":10,\"ownUserAgentHosts\":[\"*.example.com\"]}")
  ],
  globalOutbound = "internet",
);

const pluginWorker :Workerd.Worker = (
  modules = [
    (name = "index.js", esModule = embed "/modules/index.js"),
    (name = "helper.py", pythonModule = embed "/modules/helper.py"),
    (name = "@bookclerk/plugin-sdk/workerd", esModule = embed ".bookclerk/sdk-workerd.js"),
    (name = "@bookclerk/plugin-sdk", esModule = embed ".bookclerk/sdk-workerd.js"),
    (name = "bookclerk_plugin_sdk/_abi.py", pythonModule = embed ".bookclerk/sdk-product-abi.py"),
    (name = "bookclerk_plugin_sdk/_capnp.py", pythonModule = embed ".bookclerk/sdk-capnp.py"),
    (name = "bookclerk_plugin_sdk/_wire.py", pythonModule = embed ".bookclerk/sdk-wire.py"),
    (name = "bookclerk_plugin_sdk/guest_sql.py", pythonModule = embed ".bookclerk/sdk-guest-sql.py"),
    (name = "bookclerk_plugin_sdk/limits.py", pythonModule = embed ".bookclerk/sdk-limits.py"),
    (name = "bookclerk_plugin_sdk/plugin_migration.py", pythonModule = embed ".bookclerk/sdk-plugin-migration.py"),
    (name = "bookclerk_plugin_sdk/db_value.py", pythonModule = embed ".bookclerk/sdk-db-value.py"),
    (name = "bookclerk_plugin_sdk/scan_page.py", pythonModule = embed ".bookclerk/sdk-scan-page.py"),
    (name = "bookclerk_plugin_sdk/__init__.py", pythonModule = embed ".bookclerk/sdk-init.py"),
    (name = "bookclerk_plugin_sdk/workerd.py", pythonModule = embed ".bookclerk/sdk-workerd.py")
  ],
  compatibilityDate = "2026-08-01",
  compatibilityFlags = ["python_workers", "disable_python_external_sdk"],
  bindings = [],
  globalOutbound = "egress",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    (name = "adapter.js", esModule = embed ".bookclerk/adapter.js"),
    (name = "@bookclerk/plugin-sdk/workerd", esModule = embed ".bookclerk/sdk-workerd.js"),
    (name = "@bookclerk/plugin-sdk", esModule = embed ".bookclerk/sdk-workerd.js")
  ],
  compatibilityDate = "2026-08-01",
  
  bindings = [
    (name = "PLUGIN", service = (name = "plugin", entrypoint = "Main")),
    (name = "PLUGIN_CLI", service = (name = "plugin", entrypoint = "Cli")),
    (name = "PLUGIN_METADATA", service = (name = "plugin", entrypoint = "Metadata")),
    (name = "PLUGIN_DESCRIBE", json = "{\"apiVersion\":3,\"id\":\"js_main_python_helper\",\"displayName\":\"\",\"rpcFeatures\":[\"rpc.scalarLimits\",\"rpc.streams\"],\"capabilities\":{\"entrypoints\":[\"cli\",\"metadata\",\"unknown\"],\"consumes\":[],\"produces\":[],\"jobs\":[],\"databases\":[],\"bindings\":[],\"queueConsumers\":[],\"queueProducers\":[],\"crons\":[]},\"cli\":{\"commands\":[]}}"),
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
